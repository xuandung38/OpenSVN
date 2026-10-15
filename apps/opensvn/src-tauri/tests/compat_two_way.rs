//! Two-way compatibility with the svn command line client, over HTTPS (mod_dav_svn 1.14.x and
//! 1.15.0): the YAML scenarios of `tests/compat/scenarios/client/` alternate `on: engine` steps,
//! run here through the real engine and the app's [`HttpConnector`], with `on: svn14` / `on:
//! svn15` steps run by the svn clients on the same working copies. The runner then compares the
//! working copies (tree, `svn status`, `svn info` and `wc.db`) with ones svn made itself.
//!
//! svn shares libsvn_wc with TortoiseSVN, so a working copy svn 1.14 reads, updates, commits and
//! cleans up is one TortoiseSVN 1.14 handles too; `docs/tortoisesvn-checklist.md` lists what is
//! left to check by hand on Windows.
//!
//! Needs both servers and both clients on the host, so every test is `#[ignore]`:
//!
//! ```text
//! scripts/test-env/visualsvn-like.sh up      # 1.14.x on :18443, 1.15.0 on :18444
//! eval "$(scripts/test-env/visualsvn-like.sh env)"
//! export OPENSVN_COMPAT_LOCAL_URL14=https://svn.opensvn.test:18443/svn-basic
//! export OPENSVN_COMPAT_LOCAL_URL15=https://svn.opensvn.test:18444/svn-basic
//! export OPENSVN_COMPAT_SVN14_BIN=/usr/bin/svn OPENSVN_COMPAT_SVN15_BIN=/opt/svn115/bin/svn
//! cargo test -p opensvn --features real-engine,engine-conformance --test compat_two_way -- --ignored
//! ```
//!
//! The fixture repository `basic` must exist in `OPENSVN_COMPAT_LOCAL_REPOS` (see
//! `tests/compat/README.md`, "Local backend"). Each scenario runs once per server (scenario name
//! suffixed `-svn14` / `-svn15`); `OPENSVN_COMPAT_SERVERS=svn15` limits that, and
//! `OPENSVN_COMPAT_ONLY=op-lock,op-commit` runs only those files.
#![cfg(all(feature = "real-engine", feature = "engine-conformance"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use opensvn_compat::local::LocalBackend;
use opensvn_compat::runner::{EngineDriver, Runner, TEST_PASSWORD, TEST_USER};
use opensvn_compat::scenario::{Scenario, Step, SvnVersion};
use opensvn_ipc::{
    CleanupRequest, CommitRequest, Depth, ResolveChoice, ResolveRequest, Rev, RevertRequest,
    UpdateRequest, WcFormat, WcId,
};
use opensvn_lib::engine::real::{ClientRecovery, HttpConnector, RealEngine, RealEngineConfig};
use opensvn_lib::engine::{CheckoutPlan, EngineFacade, LockScope, OpContext, StatusQuery, WcRef};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject as _;
use svn_ra::transport::{MemoryTrustStore, ProxyConfig};
use svn_ra_api::{
    AuthMethod, AuthPrompter, CertDecision, CertInfo, CredentialRequest, Credentials,
};
use svn_types::{LockOwnershipStore, MemoryLockOwnership};
use tokio::sync::watch;

/// Answers every credential request with the test account; rejects unknown certificates (the
/// server certificate must chain to the test CA).
struct TesterPrompter;

#[async_trait]
impl AuthPrompter for TesterPrompter {
    async fn credentials(&self, _req: &CredentialRequest) -> Option<Credentials> {
        Some(Credentials {
            username: TEST_USER.into(),
            password: secrecy::SecretString::from(TEST_PASSWORD.to_owned()),
            save: false,
            method: AuthMethod::Basic,
        })
    }
    async fn confirm_certificate(&self, _info: &CertInfo) -> CertDecision {
        CertDecision::Reject
    }
}

fn connector(ca: &Path) -> Arc<HttpConnector> {
    let der = CertificateDer::from_pem_file(ca)
        .unwrap_or_else(|e| panic!("test CA {}: {e}", ca.display()));
    let (_settings, rx) = watch::channel(opensvn_lib::settings::defaults(None));
    let (_invalidate, inv) = watch::channel(0u64);
    Arc::new(HttpConnector::with_config_tweak(
        Arc::new(TesterPrompter),
        Arc::new(MemoryTrustStore::new()),
        rx,
        inv,
        move |c| {
            c.proxy = ProxyConfig::Disabled;
            c.extra_root_certs = vec![der.clone()];
        },
    ))
}

/// `svn`-style command lines executed by the real engine.
///
/// Supported: `checkout URL [PATH] [-r N] [--depth D] [--ignore-externals]`,
/// `update [PATH...] [-r N] [--set-depth D]`, `switch URL [-r N]`,
/// `commit -m MSG [PATH...] [--no-unlock]`, `revert [-R] PATH...`, `add PATH...`,
/// `delete [--keep-local] PATH...`, `move FROM TO`, `lock [-m MSG] [--force] PATH...`,
/// `unlock [--force] PATH...`, `cleanup [--remove-unversioned] [--remove-ignored]
/// [--vacuum-pristines]`, `resolve --accept base|working|mine-full|theirs-full PATH...`,
/// `propset NAME VALUE PATH...`, `propdel NAME PATH...`, `status` (must read the working copy).
struct EngineCli {
    rt: tokio::runtime::Runtime,
    engine: RealEngine,
    _state: tempfile::TempDir,
}

impl EngineCli {
    fn new(ca: &Path) -> Self {
        let state = tempfile::tempdir().unwrap();
        let engine = RealEngine::new(RealEngineConfig {
            connector: connector(ca),
            lock_ownership: Arc::new(MemoryLockOwnership::for_current_process("compat-boot"))
                as Arc<dyn LockOwnershipStore>,
            boot_id: "compat-boot".to_owned(),
            state_dir: state.path().to_path_buf(),
            recovery: Arc::new(ClientRecovery),
        });
        Self {
            rt: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap(),
            engine,
            _state: state,
        }
    }
}

/// Parsed command line: subcommand, options with values, flags and positional arguments.
#[derive(Debug, Default)]
struct Cmd {
    sub: String,
    message: Option<String>,
    rev: Option<Rev>,
    depth: Option<Depth>,
    set_depth: Option<Depth>,
    accept: Option<String>,
    flags: Vec<String>,
    args: Vec<String>,
}

impl Cmd {
    fn parse(argv: &[String]) -> Result<Self> {
        let (sub, rest) = argv.split_first().ok_or_else(|| anyhow!("empty command"))?;
        let mut cmd = Self {
            sub: sub.clone(),
            ..Self::default()
        };
        let mut it = rest.iter();
        while let Some(a) = it.next() {
            let mut value = || {
                it.next()
                    .cloned()
                    .ok_or_else(|| anyhow!("{a} needs a value"))
            };
            match a.as_str() {
                "-m" | "--message" => cmd.message = Some(value()?),
                "-r" | "--revision" => cmd.rev = Some(Rev(value()?.parse()?)),
                "--depth" => cmd.depth = Some(depth(&value()?)?),
                "--set-depth" => cmd.set_depth = Some(depth(&value()?)?),
                "--accept" => cmd.accept = Some(value()?),
                "-q" | "--quiet" => {}
                f if f.starts_with('-') => cmd.flags.push(f.to_owned()),
                _ => cmd.args.push(a.clone()),
            }
        }
        Ok(cmd)
    }

    fn flag(&self, names: &[&str]) -> bool {
        self.flags.iter().any(|f| names.contains(&f.as_str()))
    }

    /// Positional arguments as working copy relative paths (`.` is the root).
    fn rels(&self) -> Vec<String> {
        self.args.iter().map(|a| rel(a)).collect()
    }
}

fn depth(s: &str) -> Result<Depth> {
    Ok(match s {
        "empty" => Depth::Empty,
        "files" => Depth::Files,
        "immediates" => Depth::Immediates,
        "infinity" => Depth::Infinity,
        other => bail!("unknown depth {other}"),
    })
}

fn rel(p: &str) -> String {
    let p = p.trim_start_matches("./").trim_end_matches('/');
    if p == "." { String::new() } else { p.to_owned() }
}

fn wc_ref(dir: &Path) -> Result<WcRef> {
    Ok(WcRef {
        id: WcId("compat".to_owned()),
        root: dir
            .canonicalize()
            .with_context(|| format!("canonicalize {}", dir.display()))?,
    })
}

impl EngineCli {
    async fn exec(&self, dir: &Path, argv: &[String]) -> Result<()> {
        let c = Cmd::parse(argv)?;
        let ctx = OpContext::detached();
        let e = &self.engine;
        if c.sub == "checkout" || c.sub == "co" {
            let url = c.args.first().ok_or_else(|| anyhow!("checkout needs a URL"))?;
            let path = c.args.get(1).map_or_else(|| dir.to_path_buf(), |p| dir.join(rel(p)));
            std::fs::create_dir_all(&path)?;
            let plan = CheckoutPlan {
                url: url.clone(),
                path: path.canonicalize()?,
                rev: c.rev,
                depth: c.depth.unwrap_or(Depth::Infinity),
                sparse: None,
                format: WcFormat::V31,
                store_pristine: true,
                ignore_externals: c.flag(&["--ignore-externals"]),
            };
            e.checkout(&plan, &ctx).await?;
            return Ok(());
        }
        let wc = wc_ref(dir)?;
        let id = wc.id.clone();
        match c.sub.as_str() {
            "update" | "up" => {
                if let Some(d) = c.set_depth {
                    let paths = c.rels();
                    for p in if paths.is_empty() { vec![String::new()] } else { paths } {
                        e.set_depth(&wc, &p, d, &ctx).await?;
                    }
                } else {
                    let paths = c.rels();
                    let req = UpdateRequest {
                        wc_id: id,
                        paths: (!paths.is_empty()).then_some(paths),
                        rev: c.rev,
                        depth: c.depth,
                    };
                    e.update(&wc, &req, &ctx).await?;
                }
            }
            "switch" | "sw" => {
                let url = c.args.first().ok_or_else(|| anyhow!("switch needs a URL"))?;
                if c.args.len() > 1 && !rel(&c.args[1]).is_empty() {
                    bail!("the engine switches the working copy root only");
                }
                e.switch(&wc, url, c.rev, &ctx).await?;
            }
            "commit" | "ci" => {
                let paths = c.rels();
                let req = CommitRequest {
                    wc_id: id,
                    paths: if paths.is_empty() { vec![String::new()] } else { paths },
                    message: c.message.clone().unwrap_or_default(),
                    keep_locks: c.flag(&["--no-unlock"]),
                    auto_add_remove: false,
                };
                e.commit(&wc, &req, &ctx).await?;
            }
            "revert" => {
                let req = RevertRequest {
                    wc_id: id,
                    paths: c.rels(),
                    remove_unversioned: None,
                };
                e.revert(&wc, &req, &ctx).await?;
            }
            "add" => e.add_paths(&wc, &c.rels(), &ctx).await?,
            "delete" | "del" | "remove" | "rm" => {
                e.delete_paths(&wc, &c.rels(), c.flag(&["--keep-local"]), &ctx)
                    .await?;
            }
            "move" | "mv" | "rename" | "ren" => {
                let [from, to] = c.rels().try_into().map_err(|_| anyhow!("move FROM TO"))?;
                e.move_path(&wc, &from, &to, &ctx).await?;
            }
            "lock" => {
                let scope = LockScope::Wc {
                    wc: wc.clone(),
                    paths: c.rels(),
                };
                e.lock(&scope, c.message.as_deref(), c.flag(&["--force"]), &ctx)
                    .await?;
            }
            "unlock" => {
                let scope = LockScope::Wc {
                    wc: wc.clone(),
                    paths: c.rels(),
                };
                e.unlock(&scope, c.flag(&["--force"]), &ctx).await?;
            }
            "cleanup" => {
                let req = CleanupRequest {
                    wc_id: id,
                    remove_unversioned: Some(c.flag(&["--remove-unversioned"])),
                    remove_ignored: Some(c.flag(&["--remove-ignored"])),
                    vacuum_pristines: Some(c.flag(&["--vacuum-pristines"])),
                };
                e.cleanup(&wc, &req, &ctx).await?;
            }
            "resolve" => {
                let choice = match c.accept.as_deref() {
                    Some("base") => ResolveChoice::Base,
                    Some("working") => ResolveChoice::Working,
                    Some("mine-full") => ResolveChoice::Mine,
                    Some("theirs-full") => ResolveChoice::Theirs,
                    other => bail!("resolve --accept {other:?} is not supported"),
                };
                for path in c.rels() {
                    let req = ResolveRequest {
                        wc_id: id.clone(),
                        path,
                        choice: choice.clone(),
                    };
                    e.resolve(&wc, &req, &ctx).await?;
                }
            }
            "propset" | "pset" | "ps" => {
                let [name, value, paths @ ..] = c.args.as_slice() else {
                    bail!("propset NAME VALUE PATH...");
                };
                for p in paths {
                    e.props_set(&wc, &rel(p), name, Some(value), &ctx).await?;
                }
            }
            "propdel" | "pdel" | "pd" => {
                let [name, paths @ ..] = c.args.as_slice() else {
                    bail!("propdel NAME PATH...");
                };
                for p in paths {
                    e.props_set(&wc, &rel(p), name, None, &ctx).await?;
                }
            }
            "status" | "st" => {
                let query = StatusQuery {
                    include_ignored: false,
                    include_unversioned: true,
                };
                e.status(&wc, query, &ctx).await?;
            }
            other => bail!("engine step: unsupported subcommand {other}"),
        }
        Ok(())
    }
}

impl EngineDriver for EngineCli {
    fn run(&self, _step: &Step, wc_dir: &Path, svn_args: &[String]) -> Result<()> {
        self.rt.block_on(self.exec(wc_dir, svn_args))
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn scenario_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "yaml"))
        .collect();
    files.sort();
    files
}

fn servers() -> Vec<SvnVersion> {
    let only = std::env::var("OPENSVN_COMPAT_SERVERS").unwrap_or_default();
    [SvnVersion::V14, SvnVersion::V15]
        .into_iter()
        .filter(|v| only.is_empty() || only.split(',').any(|s| s.trim() == v.tag()))
        .collect()
}

/// Runs every scenario whose file name starts with `prefix` against every server; fails with
/// all mismatches at the end.
fn run_scenarios(prefix: &str) {
    let work = repo_root().join("target/compat/work-engine");
    let backend = LocalBackend::from_env(&work.join(".svn-config"))
        .expect("local compat environment (see the module docs)");
    let engine = EngineCli::new(backend.ca());
    let runner = Runner::new(&backend, work).with_engine(&engine);
    let mut failures = Vec::new();
    let mut ran = 0;
    for file in scenario_files(&repo_root().join("tests/compat/scenarios/client")) {
        let stem = file.file_stem().unwrap().to_string_lossy().into_owned();
        let only = std::env::var("OPENSVN_COMPAT_ONLY").unwrap_or_default();
        if !stem.starts_with(prefix) || !(only.is_empty() || only.split(',').any(|o| o == stem)) {
            continue;
        }
        let base = Scenario::load(&file).unwrap();
        for server in servers() {
            let mut scn = base.clone();
            scn.server = server;
            scn.name = format!("{}-{}", base.name, server.tag());
            ran += 1;
            match runner.run(&scn) {
                Ok(report) if report.passed() => {
                    eprintln!("PASS {} ({} steps)", scn.name, report.steps_run);
                }
                Ok(report) => {
                    eprintln!("FAIL {}", scn.name);
                    failures.extend(report.mismatches);
                }
                Err(e) => {
                    eprintln!("ERROR {}: {e:#}", scn.name);
                    failures.push(format!("[{}] {e:#}", scn.name));
                }
            }
        }
    }
    assert!(ran > 0, "no scenario matches {prefix}*");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Engine and svn take turns on the same working copies for each operation.
#[test]
#[ignore = "needs mod_dav_svn 1.14.x and 1.15.0 and both svn clients (see the module docs)"]
fn client_operations_alternate_with_svn() {
    run_scenarios("op-");
}

/// DAV interop: svn 1.14 and 1.15 run status, update, commit and cleanup on a working copy the
/// engine checked out and changed; then svn changes its own working copy and the engine updates
/// and commits it. wc.db and status are compared with a working copy svn made.
#[test]
#[ignore = "needs mod_dav_svn 1.14.x and 1.15.0 and both svn clients (see the module docs)"]
fn dav_interop_engine_and_svn_share_working_copies() {
    run_scenarios("interop-");
}

#[test]
fn engine_command_lines_parse() {
    let argv: Vec<String> = ["commit", "-m", "msg", "./a/b", ".", "--no-unlock", "-q"]
        .iter()
        .map(ToString::to_string)
        .collect();
    let c = Cmd::parse(&argv).unwrap();
    assert_eq!(c.sub, "commit");
    assert_eq!(c.message.as_deref(), Some("msg"));
    assert_eq!(c.rels(), vec!["a/b".to_owned(), String::new()]);
    assert!(c.flag(&["--no-unlock"]));
    let argv: Vec<String> = ["update", "--set-depth", "files", "-r", "3", "dir/"]
        .iter()
        .map(ToString::to_string)
        .collect();
    let c = Cmd::parse(&argv).unwrap();
    assert_eq!(c.set_depth, Some(Depth::Files));
    assert_eq!(c.rev, Some(Rev(3)));
    assert_eq!(c.rels(), vec!["dir".to_owned()]);
    assert!(Cmd::parse(&["lock".to_owned(), "-m".to_owned()]).is_err());
}
