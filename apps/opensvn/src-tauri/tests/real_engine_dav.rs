//! The shared engine suite (`engine::conformance`) against Apache + mod_dav_svn, through the real
//! engine and the app's [`HttpConnector`] (HTTPS, Basic auth, the test CA as an extra root).
//!
//! Needs a server, so every test is `#[ignore]`:
//!
//! ```text
//! cargo test -p opensvn --features real-engine,engine-conformance --test real_engine_dav -- --ignored
//! ```
//!
//! Server selection, as in `crates/svn-ra/tests/compat_write.rs`:
//!
//! - a local Apache when `OPENSVN_COMPAT_LOCAL_URL` (parent-path URL, e.g.
//!   `https://localhost:18276/svn`) and `OPENSVN_COMPAT_LOCAL_REPOS` (its `SVNParentPath`, writable
//!   by the test; scratch repositories are made with the local `svnadmin`) are set; the CA comes
//!   from `OPENSVN_COMPAT_LOCAL_CA` or `OPENSVN_COMPAT_CA` (PEM);
//! - otherwise the compat containers on `:8443` (1.14.5) and `:8444` (1.15.0)
//!   (`cargo xtask compat up --no-build`), CA `target/compat/certs/ca.crt`.
//!
//! Every test loads the standard repository of the suite into a fresh scratch repository (a dump
//! stream through `svnadmin load`, so no svn client is needed) and deletes it afterwards. The
//! server must accept the fixed account `tester` / `tester-pass` (Basic).
//!
//! The path-based authorization case needs a local server whose `SVNParentPath` location has
//! `AuthzSVNAccessFile` set to a file the test may append to, named by
//! `OPENSVN_COMPAT_LOCAL_AUTHZ` (its first rule must grant `tester` the other repositories). The
//! compat containers have no `mod_authz_svn`, so there the case reports that it was skipped.
#![cfg(all(feature = "real-engine", feature = "engine-conformance"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use async_trait::async_trait;
use opensvn_ipc::{CommitRequest, CopyLocationRequest, Depth, UpdateRequest, WcFormat, WcId};
use opensvn_lib::engine::conformance::{self, Harness, PlantedOwner};
use opensvn_lib::engine::real::{ClientRecovery, HttpConnector, RealEngine, RealEngineConfig};
use opensvn_lib::engine::{CheckoutPlan, EngineFacade, OpContext, StatusQuery, WcRef};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject as _;
use svn_ra::transport::{MemoryTrustStore, ProxyConfig};
use svn_ra_api::{
    AuthMethod, AuthPrompter, CertDecision, CertInfo, CredentialRequest, Credentials,
};
use svn_types::{ErrorCode, LockOwnershipStore, MemoryLockOwnership, RelPath, SvnError};
use svn_wc::WcDb;
use svn_wc::db::{LockOwner, ObtainMode};
use tokio::sync::watch;

const THIS_BOOT: &str = "boot-now";
const OLD_BOOT: &str = "boot-before-crash";
const DEAD_PID: u32 = 4242;

const SERVERS: [(&str, u16, &str); 2] = [
    ("1.14.5", 8443, "opensvn-compat-svn14-server-1"),
    ("1.15.0", 8444, "opensvn-compat-svn15-server-1"),
];

/// Where the scratch repositories of a server live.
#[derive(Clone, Debug)]
enum Host {
    /// In a compat container (`docker exec`), served on this port of localhost.
    Docker { container: &'static str, port: u16 },
    /// In a local `SVNParentPath` directory, served below this parent-path URL.
    Local { repos: PathBuf, base: String },
}

/// The servers to test against: the local one from the environment, or the compat containers.
fn servers() -> Vec<(String, Host)> {
    if let (Ok(base), Ok(repos)) = (
        std::env::var("OPENSVN_COMPAT_LOCAL_URL"),
        std::env::var("OPENSVN_COMPAT_LOCAL_REPOS"),
    ) {
        return vec![(
            "local".to_owned(),
            Host::Local {
                repos: PathBuf::from(repos),
                base: base.trim_end_matches('/').to_owned(),
            },
        )];
    }
    SERVERS
        .iter()
        .map(|&(version, port, container)| (version.to_owned(), Host::Docker { container, port }))
        .collect()
}

fn ca_path() -> PathBuf {
    std::env::var_os("OPENSVN_COMPAT_LOCAL_CA")
        .or_else(|| std::env::var_os("OPENSVN_COMPAT_CA"))
        .map_or_else(
            || {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../../target/compat/certs/ca.crt")
            },
            PathBuf::from,
        )
}

/// Answers every credential request with the fixed test account; rejects unknown certificates
/// (the server certificate must chain to the test CA).
struct TesterPrompter;

#[async_trait]
impl AuthPrompter for TesterPrompter {
    async fn credentials(&self, _req: &CredentialRequest) -> Option<Credentials> {
        Some(Credentials {
            username: "tester".into(),
            password: secrecy::SecretString::from("tester-pass".to_owned()),
            save: false,
            method: AuthMethod::Basic,
        })
    }
    async fn confirm_certificate(&self, _info: &CertInfo) -> CertDecision {
        CertDecision::Reject
    }
}

/// The app's connector with the test CA as an extra trusted root and no proxy.
fn connector() -> Arc<HttpConnector> {
    let ca = ca_path();
    let der = CertificateDer::from_pem_file(&ca)
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

/// Dump stream of the suite's standard repository (revision 1 by `alice`).
fn standard_dump() -> Vec<u8> {
    fn props(pairs: &[(&str, &str)]) -> String {
        let mut s = String::new();
        for (k, v) in pairs {
            s.push_str(&format!("K {}\n{k}\nV {}\n{v}\n", k.len(), v.len()));
        }
        s.push_str("PROPS-END\n");
        s
    }
    let mut out = String::from("SVN-fs-dump-format-version: 2\n\n");
    let rev_props = props(&[
        ("svn:author", "alice"),
        ("svn:date", "2026-01-01T00:00:00.000000Z"),
        ("svn:log", "Initial import"),
    ]);
    out.push_str(&format!(
        "Revision-number: 1\nProp-content-length: {0}\nContent-length: {0}\n\n{rev_props}\n",
        rev_props.len()
    ));
    let empty = props(&[]);
    for dir in ["trunk", "trunk/src", "branches", "tags"] {
        out.push_str(&format!(
            "Node-path: {dir}\nNode-kind: dir\nNode-action: add\nProp-content-length: {0}\n\
             Content-length: {0}\n\n{empty}\n\n",
            empty.len()
        ));
    }
    for (file, text) in [
        ("trunk/README.md", "hello\n"),
        ("trunk/src/main.rs", "fn main() {}\n"),
    ] {
        out.push_str(&format!(
            "Node-path: {file}\nNode-kind: file\nNode-action: add\nProp-content-length: {}\n\
             Text-content-length: {}\nContent-length: {}\n\n{empty}{text}\n\n",
            empty.len(),
            text.len(),
            empty.len() + text.len()
        ));
    }
    out.into_bytes()
}

fn run(mut cmd: Command, stdin: Option<&[u8]>) {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("{cmd:?}: {e}"));
    if let Some(data) = stdin {
        child.stdin.take().unwrap().write_all(data).unwrap();
    }
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A scratch repository with the standard tree; deleted when dropped.
struct Scratch {
    host: Host,
    name: String,
}

impl Scratch {
    fn new(host: &Host, tag: &str) -> Self {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let name = format!(
            "wp8-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        );
        let svnadmin = |args: &[&str]| -> Command {
            match host {
                // The server runs Apache as uid 33; the repository must belong to that user.
                Host::Docker { container, .. } => {
                    let mut c = Command::new("docker");
                    c.args(["exec", "-i", "-u", "33:33", container, "svnadmin"])
                        .args(args)
                        .arg(format!("/var/svn/repos/{name}"));
                    c
                }
                Host::Local { repos, .. } => {
                    let mut c = Command::new("svnadmin");
                    c.args(args).arg(repos.join(&name));
                    c
                }
            }
        };
        run(svnadmin(&["create"]), None);
        run(svnadmin(&["load", "-q"]), Some(&standard_dump()));
        Self {
            host: host.clone(),
            name,
        }
    }

    fn url(&self) -> String {
        match &self.host {
            Host::Docker { port, .. } => format!("https://localhost:{port}/svn/{}", self.name),
            Host::Local { base, .. } => format!("{base}/{}", self.name),
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        match &self.host {
            Host::Docker { container, .. } => {
                let _ = Command::new("docker")
                    .args(["exec", "-u", "33:33", container, "rm", "-rf"])
                    .arg(format!("/var/svn/repos/{}", self.name))
                    .output();
            }
            Host::Local { repos, .. } => {
                let _ = std::fs::remove_dir_all(repos.join(&self.name));
            }
        }
    }
}

/// The real engine over the app's HTTP connector, against one scratch repository.
struct DavHarness {
    engine: RealEngine,
    store: Arc<MemoryLockOwnership>,
    scratch: Scratch,
    tmp: tempfile::TempDir,
}

fn harness(host: &Host, tag: &str) -> DavHarness {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryLockOwnership::for_current_process(THIS_BOOT));
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let engine = RealEngine::new(RealEngineConfig {
        connector: connector(),
        lock_ownership: store.clone() as Arc<dyn LockOwnershipStore>,
        boot_id: THIS_BOOT.to_owned(),
        state_dir: state,
        recovery: Arc::new(ClientRecovery),
    });
    DavHarness {
        engine,
        store,
        scratch: Scratch::new(host, tag),
        tmp,
    }
}

impl Harness for DavHarness {
    fn engine(&self) -> &dyn EngineFacade {
        &self.engine
    }
    fn repo_url(&self) -> String {
        self.scratch.url()
    }
    fn work_dir(&self) -> PathBuf {
        self.tmp.path().join("work")
    }
    fn write_file(&self, root: &Path, rel: &str, content: &str) -> Result<(), SvnError> {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).map_err(|e| SvnError::from_io("write", &e))
    }
    fn remove_file(&self, root: &Path, rel: &str) -> Result<(), SvnError> {
        std::fs::remove_file(root.join(rel)).map_err(|e| SvnError::from_io("remove", &e))
    }
    fn scratch_dir(&self) -> PathBuf {
        self.tmp.path().to_path_buf()
    }
    fn plant_lock(&self, root: &Path, dir: &str, owner: PlantedOwner) -> Result<(), SvnError> {
        let db = WcDb::open(root)?;
        match owner {
            // The complete lock of an app process that died holding it.
            PlantedOwner::DeadProcess => {
                db.set_lock_owner(LockOwner {
                    store: self.store.clone() as Arc<dyn LockOwnershipStore>,
                    pid: DEAD_PID,
                    boot_id: OLD_BOOT.to_owned(),
                });
                db.wc_lock_obtain(&RelPath::new(dir)?, -1, ObtainMode::Cleanup)?;
            }
            // A bare lock row, as another client leaves it.
            PlantedOwner::Foreign => {
                let wc_id = db.wc_id();
                db.with_write(|tx| {
                    tx.execute(
                        "INSERT INTO wc_lock (wc_id, local_dir_relpath, locked_levels) \
                         VALUES (?1, ?2, -1)",
                        (wc_id, dir),
                    )
                    .map(|_| ())
                    .map_err(svn_wc::db::sql_err)
                })?;
            }
        }
        Ok(())
    }
}

macro_rules! scenario {
    ($name:ident) => {
        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "needs Apache + mod_dav_svn (see the module docs)"]
        async fn $name() {
            for (version, host) in servers() {
                eprintln!("{}: server {version}", stringify!($name));
                conformance::$name(&harness(&host, stringify!($name))).await;
            }
        }
    };
}

scenario!(repository_reads);
scenario!(commit_and_update_cycle);
scenario!(schedule_and_revert);
scenario!(conflict_blocks_commit_until_resolved);
scenario!(lock_and_unlock);
scenario!(remote_edit_and_copy);
scenario!(cancellation_and_errors);
scenario!(working_copy_metadata_operations);
scenario!(switch_to_a_branch);
scenario!(repository_content_tools);
scenario!(open_recovers_dead_locks_and_reports_foreign_ones);
scenario!(locked_unmodified_file_in_status);
scenario!(commit_with_auto_add_remove);
scenario!(update_ends_in_conflict);

scenario!(mark_move_and_set_depth);

// ---- path-based authorization ----

fn ctx() -> OpContext {
    OpContext::detached()
}

async fn checkout_trunk(h: &DavHarness, name: &str) -> WcRef {
    let plan = CheckoutPlan {
        url: format!("{}/trunk", h.repo_url()),
        path: h.work_dir().join(name),
        rev: None,
        depth: Depth::Infinity,
        sparse: None,
        format: WcFormat::V31,
        store_pristine: true,
        ignore_externals: false,
    };
    let info = h
        .engine
        .checkout(&plan, &ctx())
        .await
        .expect("checkout of trunk");
    WcRef {
        id: WcId(name.to_owned()),
        root: PathBuf::from(info.root_path),
    }
}

/// A user denied at the repository root and allowed on `/trunk` checks out, updates and commits
/// `trunk`; reads and writes outside it are refused.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn with mod_authz_svn (see the module docs)"]
async fn authz_denied_at_root_allowed_on_trunk() {
    let Ok(authz) = std::env::var("OPENSVN_COMPAT_LOCAL_AUTHZ") else {
        eprintln!("SKIPPED: OPENSVN_COMPAT_LOCAL_AUTHZ is not set (no writable authz file)");
        return;
    };
    let Some((_, host @ Host::Local { .. })) = servers().into_iter().next() else {
        eprintln!("SKIPPED: the authz case needs the local server");
        return;
    };
    let h = harness(&host, "authz");
    let name = h.scratch.name.clone();
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&authz)
            .expect("authz file");
        write!(
            f,
            "\n[{name}:/]\ntester =\n\n[{name}:/trunk]\ntester = rw\n"
        )
        .unwrap();
    }
    let url = h.repo_url();
    let e = h.engine();

    let err = e
        .repo_list(&url, None, &ctx())
        .await
        .expect_err("the root is denied");
    assert_eq!(err.code, ErrorCode::Forbidden, "list of the root: {err:?}");
    let err = e
        .repo_cat(&format!("{url}/branches"), None, &ctx())
        .await
        .expect_err("branches are denied");
    assert_eq!(
        err.code,
        ErrorCode::Forbidden,
        "read outside trunk: {err:?}"
    );

    let info = e
        .repo_info(&format!("{url}/trunk"), &ctx())
        .await
        .expect("repo_info of trunk");
    assert_eq!(info.youngest_rev.0, 1);
    let names: Vec<_> = e
        .repo_list(&format!("{url}/trunk"), None, &ctx())
        .await
        .expect("list trunk")
        .into_iter()
        .map(|x| x.name)
        .collect();
    assert!(names.contains(&"README.md".to_owned()), "{names:?}");

    let a = checkout_trunk(&h, "authz-a").await;
    let b = checkout_trunk(&h, "authz-b").await;
    e.ensure_authenticated(&a, &ctx())
        .await
        .expect("authenticate at the working copy URL");

    h.write_file(&a.root, "README.md", "hello authz\n").unwrap();
    h.write_file(&a.root, "added.txt", "added\n").unwrap();
    let rev = e
        .commit(
            &a,
            &CommitRequest {
                wc_id: a.id.clone(),
                paths: vec!["README.md".into(), "added.txt".into()],
                message: "commit below an allowed path".into(),
                keep_locks: false,
                auto_add_remove: true,
            },
            &ctx(),
        )
        .await
        .expect("commit in trunk");
    assert_eq!(rev.0, 2);

    let scan = e.status_remote(&b, &ctx()).await.expect("status_remote");
    assert_eq!(scan.latest_rev, rev);
    let reached = e
        .update(
            &b,
            &UpdateRequest {
                wc_id: b.id.clone(),
                paths: None,
                rev: None,
                depth: None,
            },
            &ctx(),
        )
        .await
        .expect("update of trunk");
    assert_eq!(reached, rev);
    let st = e
        .status(
            &b,
            StatusQuery {
                include_ignored: false,
                include_unversioned: true,
            },
            &ctx(),
        )
        .await
        .expect("status");
    assert!(st.is_empty(), "{st:?}");
    assert!(b.root.join("added.txt").exists());
    assert_eq!(
        std::fs::read_to_string(b.root.join("README.md")).unwrap(),
        "hello authz\n"
    );

    let log = e
        .log(
            &opensvn_lib::engine::LogScope::Wc {
                wc: b.clone(),
                path: String::new(),
            },
            &opensvn_ipc::LogGetRequest {
                target: opensvn_ipc::LogTarget::Wc {
                    wc_id: b.id.clone(),
                    path: String::new(),
                },
                from_rev: None,
                to_rev: None,
                limit: 10,
                stop_on_copy: None,
                with_paths: true,
                filter: None,
            },
            &ctx(),
        )
        .await
        .expect("log of trunk");
    assert_eq!(log.entries.len(), 2, "{:?}", log.entries);

    let err = e
        .copy_location(
            &CopyLocationRequest {
                from_url: format!("{url}/trunk"),
                from_rev: None,
                to_url: format!("{url}/branches/denied"),
                message: "branch where tester may not write".into(),
                switch_wc_id: None,
                from_wc: None,
            },
            None,
            &ctx(),
        )
        .await
        .expect_err("writing outside trunk is refused");
    assert_eq!(
        err.code,
        ErrorCode::Forbidden,
        "copy outside trunk: {err:?}"
    );
}
