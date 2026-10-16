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
//!
//! When the local server runs as another user (Apache as `www-data` while the tests run as root),
//! `OPENSVN_COMPAT_LOCAL_SVNADMIN` names the `svnadmin` to use (a wrapper that runs it as that
//! user); see `scripts/test-env/visualsvn-like.sh`.
//!
//! Kerberos single sign-on: with `OPENSVN_COMPAT_AUTH=negotiate` the tests answer no credential
//! prompt at all and fail when one is shown; the server must accept the Kerberos ticket of the
//! test process (`KRB5CCNAME`) through Negotiate. Build with `negotiate-gssapi` on macOS/Linux:
//!
//! ```text
//! eval "$(scripts/test-env/visualsvn-like.sh env)"
//! OPENSVN_COMPAT_AUTH=negotiate \
//!   OPENSVN_COMPAT_LOCAL_URL=https://svn.opensvn.test:18443/svn-sso \
//!   cargo test -p opensvn --features real-engine,negotiate-gssapi,engine-conformance \
//!   --test real_engine_dav -- --ignored
//! ```
//!
//! The authz case then writes its rules for the ticket's user (`OPENSVN_COMPAT_AUTHZ_USER`,
//! default `alice` for Negotiate and `tester` for Basic).
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

/// Single sign-on (`OPENSVN_COMPAT_AUTH=negotiate`): no prompt may be answered.
fn negotiate_mode() -> bool {
    std::env::var("OPENSVN_COMPAT_AUTH").is_ok_and(|v| v.eq_ignore_ascii_case("negotiate"))
}

/// User the server sees (authz rules are written for it).
fn authz_user() -> String {
    std::env::var("OPENSVN_COMPAT_AUTHZ_USER")
        .unwrap_or_else(|_| if negotiate_mode() { "alice" } else { "tester" }.to_owned())
}

/// Credential prompts shown so far (all tests of the process).
static PROMPTS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Fails when single sign-on needed a prompt.
fn assert_no_prompt() {
    if negotiate_mode() {
        let prompts = PROMPTS.lock().unwrap();
        assert!(
            prompts.is_empty(),
            "SSO showed credential prompts: {prompts:?}"
        );
    }
}

/// Answers every credential request with the fixed test account (none in Negotiate mode);
/// rejects unknown certificates (the server certificate must chain to the test CA).
struct TesterPrompter;

#[async_trait]
impl AuthPrompter for TesterPrompter {
    async fn credentials(&self, req: &CredentialRequest) -> Option<Credentials> {
        PROMPTS.lock().unwrap().push(format!("{req:?}"));
        if negotiate_mode() {
            return None;
        }
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
                    let mut c = Command::new(
                        std::env::var_os("OPENSVN_COMPAT_LOCAL_SVNADMIN")
                            .unwrap_or_else(|| "svnadmin".into()),
                    );
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

// ---- the suite through scripts/test-env/flaky-tls-proxy.py ----
//
// `OPENSVN_COMPAT_FAULTS` names the faults of a flaky proxy that `OPENSVN_COMPAT_LOCAL_URL`
// goes through (e.g. `--reset-after 2 --idle-reset 2`), `OPENSVN_COMPAT_PROXY_LOG` its log
// (written with `--log-requests`). Reads must then still succeed (they are sent again or
// resumed). A write whose connection the proxy breaks is never sent again (#166): it fails, and
// the suite checks that it failed for that reason and left nothing behind, then repeats it like
// a user would (`write_op!`). The tests that start their own proxy are skipped.

fn behind_a_faulty_proxy() -> bool {
    std::env::var("OPENSVN_COMPAT_FAULTS").is_ok_and(|f| !f.trim().is_empty())
}

fn proxy_log() -> String {
    let path = std::env::var_os("OPENSVN_COMPAT_PROXY_LOG")
        .expect("OPENSVN_COMPAT_PROXY_LOG names the log of the flaky proxy");
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Checks a write that failed with `err` after line `mark` of the proxy log:
///
/// - the proxy broke a connection of this repository while it carried a write (reset or closed
///   under it, or cut while idle right after it): the failure is the injected fault, not a
///   read that was not sent again;
/// - no write was sent twice;
/// - no transaction of the failed write is left in the repository.
fn check_broken_write(scratch: &Scratch, mark: usize, err: &SvnError) {
    let log = proxy_log();
    let lines: Vec<&str> = log.lines().skip(mark).collect();
    let conn_of = |l: &str| {
        l.strip_prefix("flaky-tls-proxy: conn ")
            .and_then(|r| r.split_once(':'))
            .map(|(c, _)| c.to_owned())
    };
    let ours: std::collections::HashSet<String> = lines
        .iter()
        .filter(|l| l.contains(": request ") && l.contains(&format!("/{}", scratch.name)))
        .filter_map(|l| conn_of(l))
        .collect();
    // Method of the request the fault hit: named by the fault line, or the last request the
    // connection carried.
    let mut last: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut hit = Vec::new();
    for l in &lines {
        let Some(conn) = conn_of(l).filter(|c| ours.contains(c)) else {
            continue;
        };
        if let Some((_, req)) = l.split_once(": request ") {
            last.insert(conn, req.split(' ').next().unwrap_or("").to_owned());
        } else if let Some(m) = l
            .split_once(" before request ")
            .and_then(|(_, r)| r.split_once('('))
            .map(|(_, m)| {
                m.trim_end_matches(')')
                    .split([',', ')'])
                    .next()
                    .unwrap_or("")
            })
        {
            hit.push(m.to_owned());
        } else if l.contains("idle") || l.contains("truncated") {
            hit.push(last.get(&conn).cloned().unwrap_or_default());
        }
    }
    assert!(
        hit.iter().any(|m| WRITE_METHODS.contains(&m.as_str())),
        "{err:?} without a write broken by the proxy (faults on: {hit:?}):\n{}",
        lines.join("\n")
    );
    let mut writes: Vec<&str> = lines
        .iter()
        .filter_map(|l| l.split_once(": request ").map(|(_, r)| r))
        .filter(|r| r.contains(&format!("/{}", scratch.name)))
        .filter(|r| {
            WRITE_METHODS
                .iter()
                .any(|m| r.starts_with(&format!("{m} ")))
        })
        .collect();
    let all = writes.len();
    writes.sort_unstable();
    writes.dedup();
    assert_eq!(
        writes.len(),
        all,
        "a write was sent twice:\n{}",
        lines.join("\n")
    );
    let txns = scratch.transactions();
    assert!(txns.is_empty(), "transactions left after {err:?}: {txns:?}");
}

impl Scratch {
    /// Names of the transactions in the repository (`svnadmin lstxns`).
    fn transactions(&self) -> Vec<String> {
        let mut c = match &self.host {
            Host::Docker { container, .. } => {
                let mut c = Command::new("docker");
                c.args(["exec", "-u", "33:33", container, "svnadmin", "lstxns"])
                    .arg(format!("/var/svn/repos/{}", self.name));
                c
            }
            Host::Local { repos, .. } => {
                let mut c = Command::new(
                    std::env::var_os("OPENSVN_COMPAT_LOCAL_SVNADMIN")
                        .unwrap_or_else(|| "svnadmin".into()),
                );
                c.arg("lstxns").arg(repos.join(&self.name));
                c
            }
        };
        let out = c.output().expect("svnadmin lstxns");
        assert!(out.status.success(), "svnadmin lstxns: {out:?}");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .filter(|l| !l.is_empty())
            .collect()
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
    fn writes_may_break(&self) -> bool {
        behind_a_faulty_proxy()
    }
    fn write_mark(&self) -> usize {
        if behind_a_faulty_proxy() {
            proxy_log().lines().count()
        } else {
            0
        }
    }
    fn check_broken_write(&self, mark: usize, err: &SvnError) {
        check_broken_write(&self.scratch, mark, err);
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
            assert_no_prompt();
        }
    };
}

scenario!(repository_reads);
scenario!(commit_and_update_cycle);
scenario!(schedule_and_revert);
scenario!(conflict_blocks_commit_until_resolved);
scenario!(lock_and_unlock);
scenario!(remote_edit_and_copy);
scenario!(repo_info_at_a_peg_revision);
scenario!(cancellation_and_errors);
scenario!(working_copy_metadata_operations);
scenario!(switch_to_a_branch);
scenario!(repository_content_tools);
scenario!(open_recovers_dead_locks_and_reports_foreign_ones);
scenario!(locked_unmodified_file_in_status);
scenario!(needs_lock_unmodified_file_in_status);
scenario!(lock_held_elsewhere_and_steal);
scenario!(commit_with_auto_add_remove);
scenario!(update_ends_in_conflict);
scenario!(update_of_sub_paths);

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
    let user = authz_user();
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&authz)
            .expect("authz file");
        write!(
            f,
            "\n[{name}:/]\n{user} =\n\n[{name}:/trunk]\n{user} = rw\n"
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
        .entries
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
    let rev = opensvn_lib::write_op!(
        &h,
        e.commit(
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
    )
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

    let err = opensvn_lib::write_op!(
        &h,
        e.copy_location(
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
    )
    .expect_err("writing outside trunk is refused");
    assert_eq!(
        err.code,
        ErrorCode::Forbidden,
        "copy outside trunk: {err:?}"
    );
    assert_no_prompt();
}

// ---- issue #111: commit below a switched subtree ----

/// A commit with changes outside and below a switched folder goes through, like `svn commit`
/// (the session moves to the common ancestor of the changes).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn (see the module docs)"]
async fn commit_with_changes_below_a_switched_subtree() {
    use opensvn_lib::engine::real::SessionConnector as _;
    for (version, host) in servers() {
        eprintln!("server {version}");
        let h = harness(&host, "switched-commit");
        let url = h.repo_url();
        let e = h.engine();
        opensvn_lib::write_op!(
            &h,
            e.copy_location(
                &CopyLocationRequest {
                    from_url: format!("{url}/trunk/src"),
                    from_rev: None,
                    to_url: format!("{url}/branches/b"),
                    message: "Branch src".into(),
                    switch_wc_id: None,
                    from_wc: None,
                },
                None,
                &ctx(),
            )
        )
        .expect("branch");
        let root = h.work_dir().join("switched");
        let info = e
            .checkout(
                &CheckoutPlan {
                    url: format!("{url}/trunk"),
                    path: root.clone(),
                    rev: None,
                    depth: Depth::Infinity,
                    sparse: None,
                    format: WcFormat::V31,
                    store_pristine: true,
                    ignore_externals: false,
                },
                &ctx(),
            )
            .await
            .expect("checkout");
        let wc = WcRef {
            id: WcId("switched".into()),
            root: PathBuf::from(info.root_path),
        };
        // Switch `src` with the library directly (the facade switches whole working copies),
        // over a session anchored at the folder.
        let cancel = svn_types::CancelToken::new();
        let src_url = svn_types::RepoUrl::parse(&format!("{url}/trunk/src")).unwrap();
        let session = connector()
            .connect_with(
                &src_url,
                &opensvn_lib::engine::real::ConnectOptions::at_url(&ctx()),
                &cancel,
            )
            .await
            .expect("session");
        let client = svn_client::ClientCtx::new(LockOwner {
            store: h.store.clone() as Arc<dyn LockOwnershipStore>,
            pid: std::process::id(),
            boot_id: THIS_BOOT.to_owned(),
        });
        svn_client::switch(
            &client,
            session.as_ref(),
            &wc.root.join("src"),
            &svn_types::RepoUrl::parse(&format!("{url}/branches/b")).unwrap(),
            None,
            svn_types::Depth::Unknown,
            // A copy of trunk/src: the ancestry check passes on the real server.
            false,
            &cancel,
        )
        .await
        .expect("switch src");

        h.write_file(&wc.root, "README.md", "hello trunk\n")
            .unwrap();
        h.write_file(&wc.root, "src/main.rs", "fn main() { branch(); }\n")
            .unwrap();
        let rev = opensvn_lib::write_op!(
            &h,
            e.commit(
                &wc,
                &CommitRequest {
                    wc_id: wc.id.clone(),
                    paths: vec![String::new()],
                    message: "Both sides".into(),
                    keep_locks: false,
                    auto_add_remove: false,
                },
                &ctx(),
            )
        )
        .expect("commit");
        for (path, expected) in [
            ("trunk/README.md", "hello trunk\n"),
            ("branches/b/main.rs", "fn main() { branch(); }\n"),
            ("trunk/src/main.rs", "fn main() {}\n"),
        ] {
            let text = e
                .repo_cat(&format!("{url}/{path}"), Some(rev), &ctx())
                .await
                .unwrap_or_else(|err| panic!("cat {path}: {err}"));
            assert_eq!(text.content, expected, "{path} at r{}", rev.0);
        }

        // Changes below the switched subtree alone: the session is anchored at the branch,
        // condensed over the items like svn.
        h.write_file(&wc.root, "src/main.rs", "fn main() { again(); }\n")
            .unwrap();
        h.write_file(&wc.root, "src/new.rs", "fn new() {}\n")
            .unwrap();
        let rev = opensvn_lib::write_op!(
            &h,
            e.commit(
                &wc,
                &CommitRequest {
                    wc_id: wc.id.clone(),
                    paths: vec!["src".into()],
                    message: "Branch only".into(),
                    keep_locks: false,
                    auto_add_remove: true,
                },
                &ctx(),
            )
        )
        .expect("switched-only commit");
        for (path, expected) in [
            ("branches/b/main.rs", "fn main() { again(); }\n"),
            ("branches/b/new.rs", "fn new() {}\n"),
            ("trunk/src/main.rs", "fn main() {}\n"),
        ] {
            let text = e
                .repo_cat(&format!("{url}/{path}"), Some(rev), &ctx())
                .await
                .unwrap_or_else(|err| panic!("cat {path}: {err}"));
            assert_eq!(text.content, expected, "{path} at r{}", rev.0);
        }
    }
    assert_no_prompt();
}

// ---- switch ancestry ----

/// The server's `get-location-segments` answer follows a copy back to its source with a gap
/// for the revisions in between, and the engine's switch checks ancestry with it like
/// `svn switch`: an unrelated location is refused (E195012) without touching the working copy,
/// a branch copied from trunk is taken, and `ignore_ancestry` takes the unrelated one too.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn (see the module docs)"]
async fn switch_checks_ancestry_with_location_segments() {
    use opensvn_lib::engine::real::SessionConnector as _;
    use svn_types::Revnum;
    for (version, host) in servers() {
        eprintln!("server {version}");
        let h = harness(&host, "switch-ancestry");
        let url = h.repo_url();
        let e = h.engine();
        // r2: `other`, the same layout as trunk added on its own.
        h.scratch.load(&add_dump(
            &["other", "other/src"],
            &[
                ("other/README.md".to_owned(), b"hello\n".to_vec()),
                ("other/src/main.rs".to_owned(), b"fn other() {}\n".to_vec()),
            ],
        ));
        // r3: a branch of trunk as it was in r1.
        opensvn_lib::write_op!(
            &h,
            e.copy_location(
                &CopyLocationRequest {
                    from_url: format!("{url}/trunk"),
                    from_rev: Some(opensvn_ipc::Rev(1)),
                    to_url: format!("{url}/branches/rel"),
                    message: "Branch".into(),
                    switch_wc_id: None,
                    from_wc: None,
                },
                None,
                &ctx(),
            )
        )
        .expect("branch");

        let cancel = svn_types::CancelToken::new();
        let session = connector()
            .connect_with(
                &svn_types::RepoUrl::parse(&url).unwrap(),
                &opensvn_lib::engine::real::ConnectOptions::at_url(&ctx()),
                &cancel,
            )
            .await
            .expect("session");
        let segments = |path: &str, start: Option<u64>, end: Option<u64>| {
            let (session, cancel) = (session.clone(), cancel.clone());
            let path = RelPath::new(path).unwrap();
            async move {
                session
                    .get_location_segments(&path, None, start.map(Revnum), end.map(Revnum), &cancel)
                    .await
                    .map(|v| {
                        v.into_iter()
                            .map(|s| {
                                (
                                    s.path.map(|p| p.as_str().to_owned()),
                                    s.range_start.0,
                                    s.range_end.0,
                                )
                            })
                            .collect::<Vec<_>>()
                    })
            }
        };
        let some = |p: &str| Some(p.to_owned());
        assert_eq!(
            segments("branches/rel/src/main.rs", None, None)
                .await
                .unwrap(),
            vec![
                (some("branches/rel/src/main.rs"), 3, 3),
                (None, 2, 2),
                (some("trunk/src/main.rs"), 1, 1),
            ]
        );
        assert_eq!(
            segments("branches/rel", Some(3), Some(2)).await.unwrap(),
            vec![(some("branches/rel"), 3, 3), (None, 2, 2)]
        );
        assert_eq!(
            segments("other", None, None).await.unwrap(),
            vec![(some("other"), 2, 3)]
        );
        assert_eq!(
            segments("", None, None).await.unwrap(),
            vec![(some(""), 0, 3)]
        );
        let err = segments("nowhere", None, None).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::PathNotFound, "{err}");

        let wc = checkout_trunk(&h, "switch-ancestry").await;
        h.write_file(&wc.root, "README.md", "hello\nlocal edit\n")
            .unwrap();
        let only_the_edit = |list: Vec<opensvn_ipc::FileStatus>| {
            list.into_iter()
                .filter(|s| s.text != opensvn_ipc::NodeStatus::Normal || s.tree_conflict)
                .map(|s| (s.path, s.text, s.tree_conflict))
                .collect::<Vec<_>>()
        };
        let query = StatusQuery {
            include_ignored: false,
            include_unversioned: true,
        };
        let edited = vec![(
            "README.md".to_owned(),
            opensvn_ipc::NodeStatus::Modified,
            false,
        )];

        let err = e
            .switch(&wc, &format!("{url}/other"), None, false, &ctx())
            .await
            .expect_err("an unrelated switch is refused");
        assert_eq!(err.code, ErrorCode::UnrelatedResources, "{err}");
        assert_eq!(err.svn_code, Some(195_012), "{err}");
        let info = e.wc_info(&wc.root, &ctx()).await.expect("info");
        assert_eq!(info.url, format!("{url}/trunk"));
        assert_eq!(
            only_the_edit(e.status(&wc, query, &ctx()).await.expect("status")),
            edited
        );

        e.switch(&wc, &format!("{url}/branches/rel"), None, false, &ctx())
            .await
            .expect("switch to the branch");
        let info = e.wc_info(&wc.root, &ctx()).await.expect("info");
        assert_eq!(info.url, format!("{url}/branches/rel"));
        assert_eq!(
            only_the_edit(e.status(&wc, query, &ctx()).await.expect("status")),
            edited
        );

        e.switch(&wc, &format!("{url}/other"), None, true, &ctx())
            .await
            .expect("switch ignoring ancestry");
        let info = e.wc_info(&wc.root, &ctx()).await.expect("info");
        assert_eq!(info.url, format!("{url}/other"));
        // Ignoring ancestry, the server sends changes, not a replacement: the local edit is
        // merged into the other README, no tree conflict.
        assert_eq!(
            only_the_edit(e.status(&wc, query, &ctx()).await.expect("status")),
            edited
        );
        assert_eq!(
            std::fs::read_to_string(wc.root.join("src/main.rs")).unwrap(),
            "fn other() {}\n"
        );
    }
    assert_no_prompt();
}

// ---- update and second switch of a switched folder (#170 review) ----

/// Lines of the servers' error logs (`OPENSVN_COMPAT_LOCAL_ERROR_LOGS`, separated by `:`) that
/// show a crashed child: Apache's `AH00052 ... exit signal` (logged at `core:notice`) and a
/// failed assertion the child wrote to stderr. `None` without the variable (no local server).
fn crashed_children() -> Option<usize> {
    let logs = std::env::var("OPENSVN_COMPAT_LOCAL_ERROR_LOGS").ok()?;
    Some(
        logs.split(':')
            .filter(|p| !p.is_empty())
            .map(|p| {
                std::fs::read_to_string(p)
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| l.contains("AH00052") || l.contains("Assertion"))
                    .count()
            })
            .sum(),
    )
}

/// A folder switched away from its parent's URL, with an excluded child, a child at another
/// revision and a nested switched child, is updated and switched again over a session at the
/// working copy root. The drive must be anchored at the folder (svn's actual-target split): a
/// `link_path` for it followed by the entries below it makes svn_repos read freed memory, and
/// mod_dav_svn 1.14 and 1.15 abort the child on every attempt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn (see the module docs)"]
async fn update_and_switch_again_of_a_switched_folder_with_mixed_children() {
    use opensvn_lib::engine::real::SessionConnector as _;
    use svn_types::{CancelToken, RepoUrl, Revnum};
    use svn_wc::db::Presence;
    let crashed_before = crashed_children();
    if crashed_before.is_none() {
        eprintln!("OPENSVN_COMPAT_LOCAL_ERROR_LOGS is not set: server crashes are not checked");
    }
    for (version, host) in servers() {
        eprintln!("server {version}");
        let h = harness(&host, "switched-again");
        let url = h.repo_url();
        let e = h.engine();
        let root = h.work_dir().join("wc");
        let info = e
            .checkout(
                &CheckoutPlan {
                    url: format!("{url}/trunk"),
                    path: root.clone(),
                    rev: None,
                    depth: Depth::Infinity,
                    sparse: None,
                    format: WcFormat::V31,
                    store_pristine: true,
                    ignore_externals: false,
                },
                &ctx(),
            )
            .await
            .expect("checkout");
        let wc = WcRef {
            id: WcId("switched-again".into()),
            root: PathBuf::from(info.root_path),
        };
        for (rel, text) in [
            ("src/x/a.txt", "a\n"),
            ("src/y/b.txt", "b\n"),
            ("src/n/c.txt", "c\n"),
        ] {
            h.write_file(&wc.root, rel, text).unwrap();
        }
        let commit = |paths: Vec<String>, message: &str| CommitRequest {
            wc_id: wc.id.clone(),
            paths,
            message: message.into(),
            keep_locks: false,
            auto_add_remove: true,
        };
        opensvn_lib::write_op!(
            &h,
            e.commit(&wc, &commit(vec![String::new()], "x, y, n"), &ctx())
        )
        .expect("commit x, y, n");
        let branch = |to: &'static str| CopyLocationRequest {
            from_url: format!("{url}/trunk/src"),
            from_rev: None,
            to_url: format!("{url}/branches/{to}"),
            message: format!("branch {to}"),
            switch_wc_id: None,
            from_wc: None,
        };
        let rev_b = opensvn_lib::write_op!(&h, e.copy_location(&branch("b"), None, &ctx()))
            .expect("branch b");
        opensvn_lib::write_op!(&h, e.copy_location(&branch("c"), None, &ctx())).expect("branch c");

        let cancel = CancelToken::new();
        let at = |u: String| {
            let cancel = cancel.clone();
            async move {
                connector()
                    .connect_with(
                        &RepoUrl::parse(&u).unwrap(),
                        &opensvn_lib::engine::real::ConnectOptions::at_url(&ctx()),
                        &cancel,
                    )
                    .await
                    .expect("session")
            }
        };
        let client = svn_client::ClientCtx::new(LockOwner {
            store: h.store.clone() as Arc<dyn LockOwnershipStore>,
            pid: std::process::id(),
            boot_id: THIS_BOOT.to_owned(),
        });
        let branch_b = RepoUrl::parse(&format!("{url}/branches/b")).unwrap();
        let trunk = at(format!("{url}/trunk")).await;
        svn_client::switch(
            &client,
            trunk.as_ref(),
            &wc.root.join("src"),
            &branch_b,
            None,
            svn_types::Depth::Unknown,
            // Branches of trunk/src: related, so the ancestry check passes.
            false,
            &cancel,
        )
        .await
        .expect("first switch of src");
        // An excluded child.
        svn_client::sparse::set_depth(
            &client,
            trunk.as_ref(),
            &wc.root.join("src/x"),
            None,
            svn_types::Depth::Exclude,
            &cancel,
        )
        .await
        .expect("exclude src/x");
        // The reviewer's repro: an update of the switched folder with an excluded child.
        svn_client::update(
            &client,
            trunk.as_ref(),
            &wc.root.join("src"),
            None,
            svn_types::Depth::Unknown,
            &cancel,
        )
        .await
        .expect("update of the switched src with an excluded child");
        // A change on the branch, so the later update has something to bring.
        h.write_file(&wc.root, "src/y/b.txt", "b on the branch\n")
            .unwrap();
        opensvn_lib::write_op!(
            &h,
            e.commit(&wc, &commit(vec!["src".into()], "y on b"), &ctx())
        )
        .expect("commit on the branch");
        // A child at another revision (anchored at its switched parent, as svn does).
        svn_client::update(
            &client,
            trunk.as_ref(),
            &wc.root.join("src/y"),
            Some(Revnum(rev_b.0)),
            svn_types::Depth::Unknown,
            &cancel,
        )
        .await
        .expect("src/y back to the branch point");
        // A nested switched child.
        svn_client::switch(
            &client,
            trunk.as_ref(),
            &wc.root.join("src/n"),
            &RepoUrl::parse(&format!("{url}/branches/c/n")).unwrap(),
            None,
            svn_types::Depth::Unknown,
            // Branches of trunk/src: related, so the ancestry check passes.
            false,
            &cancel,
        )
        .await
        .expect("switch src/n");
        let base = |rel: &str| {
            WcDb::open(&wc.root)
                .unwrap()
                .read_base(&RelPath::new(rel).unwrap())
                .unwrap()
                .unwrap_or_else(|| panic!("{rel}: no BASE row"))
        };
        // Update of the switched folder over a session at the working copy root.
        svn_client::update(
            &client,
            trunk.as_ref(),
            &wc.root.join("src"),
            None,
            svn_types::Depth::Unknown,
            &cancel,
        )
        .await
        .expect("update of the switched src");
        assert_eq!(
            std::fs::read_to_string(wc.root.join("src/y/b.txt")).unwrap(),
            "b on the branch\n"
        );
        assert_eq!(base("src").repos_relpath.as_deref(), Some("branches/b"));
        assert_eq!(base("src/x").presence, Presence::Excluded);
        assert!(!wc.root.join("src/x").exists());
        assert_eq!(base("src/n").repos_relpath.as_deref(), Some("branches/c/n"));

        // Switch it again to the same branch, the same way.
        svn_client::switch(
            &client,
            trunk.as_ref(),
            &wc.root.join("src"),
            &branch_b,
            None,
            svn_types::Depth::Unknown,
            // Branches of trunk/src: related, so the ancestry check passes.
            false,
            &cancel,
        )
        .await
        .expect("second switch of src");
        assert_eq!(base("src").repos_relpath.as_deref(), Some("branches/b"));
        assert_eq!(
            std::fs::read_to_string(wc.root.join("src/y/b.txt")).unwrap(),
            "b on the branch\n"
        );
    }
    // The requests must not have crashed the server: a malformed report can make svn_repos
    // read freed memory and abort the child while the client sees a retried success.
    assert_eq!(
        crashed_children(),
        crashed_before,
        "a request crashed a mod_dav_svn child (see the server's error log)"
    );
    assert_no_prompt();
}

// ---- connections closed or recycled by the server ----

/// Dump of one more revision with `dirs` x `files` files of `size` bytes below `trunk/bulk`.
fn bulk_dump(dirs: usize, files: usize, size: usize) -> Vec<u8> {
    let empty = "PROPS-END\n";
    let rev_props = "K 10\nsvn:author\nV 5\nalice\nK 8\nsvn:date\nV 27\n\
                     2026-01-02T00:00:00.000000Z\nK 7\nsvn:log\nV 4\nBulk\nPROPS-END\n";
    let mut out = format!(
        "SVN-fs-dump-format-version: 2\n\nRevision-number: 2\nProp-content-length: {0}\n\
         Content-length: {0}\n\n{rev_props}\n",
        rev_props.len()
    );
    let add_dir = |out: &mut String, path: &str| {
        out.push_str(&format!(
            "Node-path: {path}\nNode-kind: dir\nNode-action: add\nProp-content-length: {0}\n\
             Content-length: {0}\n\n{empty}\n\n",
            empty.len()
        ));
    };
    add_dir(&mut out, "trunk/bulk");
    for d in 0..dirs {
        add_dir(&mut out, &format!("trunk/bulk/d{d:03}"));
        for f in 0..files {
            let line = format!("dir {d} file {f}\n");
            let text: String = line.repeat(size / line.len() + 1)[..size].to_owned();
            out.push_str(&format!(
                "Node-path: trunk/bulk/d{d:03}/f{f:03}.txt\nNode-kind: file\nNode-action: add\n\
                 Prop-content-length: {}\nText-content-length: {}\nContent-length: {}\n\n\
                 {empty}{text}\n\n",
                empty.len(),
                text.len(),
                empty.len() + text.len()
            ));
        }
    }
    out.into_bytes()
}

impl Scratch {
    /// Loads one more revision from `dump` (renumbered after the youngest).
    fn load(&self, dump: &[u8]) {
        let mut c = match &self.host {
            Host::Docker { container, .. } => {
                let mut c = Command::new("docker");
                c.args(["exec", "-i", "-u", "33:33", container, "svnadmin"]);
                c
            }
            Host::Local { .. } => Command::new(
                std::env::var_os("OPENSVN_COMPAT_LOCAL_SVNADMIN")
                    .unwrap_or_else(|| "svnadmin".into()),
            ),
        };
        c.args(["load", "-q"]);
        match &self.host {
            Host::Docker { .. } => c.arg(format!("/var/svn/repos/{}", self.name)),
            Host::Local { repos, .. } => c.arg(repos.join(&self.name)),
        };
        run(c, Some(dump));
    }
}

/// Files below `root`, `.svn` excluded.
fn count_files(root: &Path) -> usize {
    let mut n = 0;
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == ".svn" {
            continue;
        }
        if entry.file_type().unwrap().is_dir() {
            n += count_files(&entry.path());
        } else {
            n += 1;
        }
    }
    n
}

/// A large checkout and repeated remote status checks ("check for updates") of another working
/// copy run at the same time over the app's one shared transport, as in the app. Against a server
/// that recycles connections often (`VSVN_KEEPALIVE_TIMEOUT=1 VSVN_MAX_KEEPALIVE_REQUESTS=5`) or
/// through `scripts/test-env/flaky-tls-proxy.py`, both must succeed and the checkout must be
/// complete and clean.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn (see the module docs)"]
async fn checkout_and_remote_status_at_the_same_time() {
    const DIRS: usize = 40;
    const FILES: usize = 25;
    for (version, host) in servers() {
        eprintln!("server {version}");
        let h = harness(&host, "concurrent");
        let e = h.engine();
        let small = checkout_trunk(&h, "small").await;
        h.scratch.load(&bulk_dump(DIRS, FILES, 4096));
        let plan = CheckoutPlan {
            url: format!("{}/trunk", h.repo_url()),
            path: h.work_dir().join("big"),
            rev: None,
            depth: Depth::Infinity,
            sparse: None,
            format: WcFormat::V31,
            store_pristine: true,
            ignore_externals: false,
        };
        let op = ctx();
        let checkout = e.checkout(&plan, &op);
        let checks = async {
            let mut scans = Vec::new();
            for _ in 0..8 {
                scans.push(e.status_remote(&small, &ctx()).await);
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
            scans
        };
        let (info, scans) = tokio::join!(checkout, checks);
        let info = info.unwrap_or_else(|err| panic!("checkout next to remote status: {err:?}"));
        for scan in scans {
            let scan = scan.unwrap_or_else(|err| panic!("remote status next to checkout: {err:?}"));
            assert_eq!(scan.incoming, 1, "{scan:?}");
        }
        let root = PathBuf::from(info.root_path);
        assert_eq!(count_files(&root), DIRS * FILES + 2);
        let big = WcRef {
            id: WcId("big".into()),
            root,
        };
        let query = StatusQuery {
            include_ignored: false,
            include_unversioned: true,
        };
        let changed: Vec<_> = e
            .status(&big, query, &ctx())
            .await
            .expect("status of the checkout")
            .into_iter()
            .filter(|s| s.text != opensvn_ipc::NodeStatus::Normal)
            .collect();
        assert!(changed.is_empty(), "{changed:?}");
    }
    assert_no_prompt();
}

/// Dump of one more revision that adds the directories `dirs` and the files `files`.
fn add_dump(dirs: &[&str], files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let empty = "PROPS-END\n";
    let rev_props = "K 10\nsvn:author\nV 5\nalice\nK 8\nsvn:date\nV 27\n\
                     2026-01-02T00:00:00.000000Z\nK 7\nsvn:log\nV 5\nStall\nPROPS-END\n";
    let mut out = format!(
        "SVN-fs-dump-format-version: 2\n\nRevision-number: 2\nProp-content-length: {0}\n\
         Content-length: {0}\n\n{rev_props}\n",
        rev_props.len(),
    )
    .into_bytes();
    for dir in dirs {
        out.extend_from_slice(
            format!(
                "Node-path: {dir}\nNode-kind: dir\nNode-action: add\nProp-content-length: {0}\n\
                 Content-length: {0}\n\n{empty}\n\n",
                empty.len()
            )
            .as_bytes(),
        );
    }
    for (path, text) in files {
        out.extend_from_slice(
            format!(
                "Node-path: {path}\nNode-kind: file\nNode-action: add\n\
                 Prop-content-length: {}\nText-content-length: {}\nContent-length: {}\n\n{empty}",
                empty.len(),
                text.len(),
                empty.len() + text.len()
            )
            .as_bytes(),
        );
        out.extend_from_slice(text);
        out.extend_from_slice(b"\n\n");
    }
    out
}

/// `len` incompressible bytes (svndiff compression must not shrink them below a cut offset).
fn noise(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// `scripts/test-env/flaky-tls-proxy.py` in front of the local server; killed when dropped.
struct FlakyProxy {
    child: std::process::Child,
    log: PathBuf,
    port: u16,
}

impl FlakyProxy {
    /// Starts the proxy on a free port in front of the server of `base` (an `https` URL of the
    /// local server), with the fault arguments `faults`. `None` when the test certificates of
    /// `scripts/test-env/visualsvn-like.sh` are not next to the CA.
    fn start(base: &str, faults: &[&str], log: PathBuf) -> Option<Self> {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        Self::spawn(base, port, faults, log)
    }

    /// [`Self::start`] on `port`; `None` while the port is still taken.
    fn start_on(base: &str, port: u16, faults: &[&str], log: PathBuf) -> Option<Self> {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_err() {
            return None;
        }
        Self::spawn(base, port, faults, log)
    }

    fn spawn(base: &str, port: u16, faults: &[&str], log: PathBuf) -> Option<Self> {
        let certs = ca_path().parent()?.to_path_buf();
        let (cert, key) = (certs.join("server.crt"), certs.join("server.key"));
        if !cert.exists() || !key.exists() {
            return None;
        }
        let url = url::Url::parse(base).ok()?;
        let upstream = format!("{}:{}", url.host_str()?, url.port_or_known_default()?);
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../scripts/test-env/flaky-tls-proxy.py");
        let child = Command::new("python3")
            .arg(script)
            .args(["--listen", &port.to_string(), "--upstream", &upstream])
            .arg("--cert")
            .arg(&cert)
            .arg("--key")
            .arg(&key)
            .arg("--ca")
            .arg(ca_path())
            .args(faults)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .expect("python3 for the flaky proxy");
        let started = std::time::Instant::now();
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "the flaky proxy did not start"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Some(Self { child, log, port })
    }

    /// `base` with the proxy's port.
    fn url(&self, base: &str) -> String {
        let mut url = url::Url::parse(base).unwrap();
        url.set_port(Some(self.port)).unwrap();
        url.as_str().trim_end_matches('/').to_owned()
    }

    /// Responses the proxy cut off so far.
    fn cuts(&self) -> usize {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains("truncated a"))
            .count()
    }
}

impl Drop for FlakyProxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A proxy that always cuts the update at the same offset (inside a file bigger than the
/// offset, so no attempt gets past it). The working copy has a conflicted directory that every
/// drive is sent again and skips, so every resumed drive notifies something, but commits nothing
/// new: two such resumes end the update with the connection error (counting notifications, it
/// went on for 20). The working copy stays resumable: once the connection works, an update
/// finishes it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn and scripts/test-env/visualsvn-like.sh certificates"]
async fn update_cut_at_the_same_offset_stops_after_two_stalled_resumes() {
    const SMALL: usize = 12;
    const BIG: usize = 2 << 20;
    if behind_a_faulty_proxy() {
        eprintln!("SKIPPED: starts its own flaky proxy, so it runs against the server directly");
        return;
    }
    let Some((_, host @ Host::Local { .. })) = servers().into_iter().next() else {
        eprintln!("SKIPPED: the flaky proxy runs in front of the local server only");
        return;
    };
    let Host::Local { base, .. } = &host else {
        unreachable!()
    };
    let h = harness(&host, "stall");
    let e = h.engine();
    h.scratch.load(&add_dump(
        &["trunk/stall", "trunk/stall/aaa"],
        &[("trunk/stall/aaa/a.txt".to_owned(), b"a\n".to_vec())],
    ));
    let logs = h.scratch_dir();
    let Some(healthy) = FlakyProxy::start(base, &[], logs.join("proxy-1.log")) else {
        eprintln!("SKIPPED: no server certificate next to the CA for the flaky proxy");
        return;
    };
    let port = healthy.port;
    let checkout = |name: &'static str, url: String| {
        let plan = CheckoutPlan {
            url,
            path: h.work_dir().join(name),
            rev: None,
            depth: Depth::Infinity,
            sparse: None,
            format: WcFormat::V31,
            store_pristine: true,
            ignore_externals: false,
        };
        async move {
            let info = e.checkout(&plan, &ctx()).await.expect("checkout");
            WcRef {
                id: WcId(name.to_owned()),
                root: PathBuf::from(info.root_path),
            }
        }
    };
    let update_req = |wc: &WcRef| UpdateRequest {
        wc_id: wc.id.clone(),
        paths: None,
        rev: None,
        depth: None,
    };
    // `b` goes through the proxy, `a` straight to the server.
    let b = checkout(
        "b",
        format!("{}/{}/trunk/stall", healthy.url(base), h.scratch.name),
    )
    .await;
    let a = checkout("a", format!("{}/trunk/stall", h.repo_url())).await;
    // A property conflict on `aaa` in `b`: every later update skips `aaa` and says so.
    e.props_set(&a, "aaa", "color", Some("theirs"), &ctx())
        .await
        .expect("propset in a");
    e.commit(
        &a,
        &CommitRequest {
            wc_id: a.id.clone(),
            paths: vec!["aaa".into()],
            message: "color".into(),
            keep_locks: false,
            auto_add_remove: true,
        },
        &ctx(),
    )
    .await
    .expect("commit from a");
    e.props_set(&b, "aaa", "color", Some("mine"), &ctx())
        .await
        .expect("propset in b");
    e.update(&b, &update_req(&b), &ctx())
        .await
        .expect("update into the conflict");
    // A change in `aaa` (so the server sends `aaa` again on every drive), small files and,
    // after them, a file bigger than the cut offset.
    let mut files = vec![("trunk/stall/aaa/new.txt".to_owned(), b"new\n".to_vec())];
    for f in 0..SMALL {
        files.push((format!("trunk/stall/zzz/s{f:03}.txt"), vec![b'x'; 1024]));
    }
    files.push(("trunk/stall/zzz/z-big.bin".to_owned(), noise(BIG)));
    h.scratch.load(&add_dump(&["trunk/stall/zzz"], &files));
    drop(healthy);

    let cutting = (0..50)
        .find_map(|_| {
            let p = FlakyProxy::start_on(
                base,
                port,
                &[
                    "--truncate-every",
                    "1",
                    "--truncate-at",
                    "262144",
                    "--truncate-method",
                    "REPORT,GET",
                ],
                logs.join("proxy-2.log"),
            );
            if p.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            p
        })
        .expect("the cutting proxy on the same port");
    let started = std::time::Instant::now();
    let err = e
        .update(&b, &update_req(&b), &ctx())
        .await
        .expect_err("an update that never gets past the cut");
    assert!(
        err.connection_failure().is_some(),
        "connection failure expected: {err:?}"
    );
    // One cut per drive: the first drive, then the two resumes that committed nothing new.
    assert_eq!(
        cutting.cuts(),
        3,
        "{}",
        std::fs::read_to_string(&cutting.log).unwrap_or_default()
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(60));
    assert!(
        !b.root.join("zzz/z-big.bin").exists(),
        "no partial file in the working copy"
    );
    drop(cutting);

    // The connection works again (same address, no faults): "Update to finish" completes it.
    let _healthy = (0..50)
        .find_map(|_| {
            let p = FlakyProxy::start_on(base, port, &[], logs.join("proxy-3.log"));
            if p.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            p
        })
        .expect("a plain proxy on the same port");
    e.update(&b, &update_req(&b), &ctx())
        .await
        .expect("update finishes");
    assert_eq!(
        std::fs::metadata(b.root.join("zzz/z-big.bin"))
            .unwrap()
            .len(),
        BIG as u64
    );
    assert_eq!(count_files(&b.root.join("zzz")), SMALL + 1);
    let changed: Vec<_> = e
        .status(
            &b,
            StatusQuery {
                include_ignored: false,
                include_unversioned: true,
            },
            &ctx(),
        )
        .await
        .expect("status")
        .into_iter()
        .filter(|s| {
            s.text != opensvn_ipc::NodeStatus::Normal
                || !matches!(
                    s.props,
                    opensvn_ipc::PropStatus::None | opensvn_ipc::PropStatus::Normal
                )
        })
        .collect();
    // Only the conflicted directory (still skipped until resolved) differs.
    assert!(
        changed.iter().all(|s| s.path.starts_with("aaa")),
        "{changed:?}"
    );
    assert_no_prompt();
}

/// The request lines the proxy logged with `--log-requests`, as `(method, uri)`.
fn requests(proxy: &FlakyProxy) -> Vec<(String, String)> {
    std::fs::read_to_string(&proxy.log)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once(": request ").map(|(_, r)| r))
        .filter_map(|r| r.split_once(' '))
        .map(|(m, u)| (m.to_owned(), u.to_owned()))
        .collect()
}

const WRITE_METHODS: [&str; 11] = [
    "MKACTIVITY",
    "POST",
    "CHECKOUT",
    "PUT",
    "PROPPATCH",
    "MKCOL",
    "COPY",
    "DELETE",
    "MERGE",
    "LOCK",
    "UNLOCK",
];

/// A proxy that silently drops connections idle for 1.5 s (and resets them under the next
/// request, which it does not forward), with pauses longer than that between operations: the
/// commit, lock, unlock and copy succeed, every write is sent once, and the stale connections
/// only ever meet reads (which are sent again on a new connection).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn and scripts/test-env/visualsvn-like.sh certificates"]
async fn writes_after_a_pause_survive_a_proxy_that_dropped_the_idle_connection() {
    if behind_a_faulty_proxy() {
        eprintln!("SKIPPED: starts its own flaky proxy, so it runs against the server directly");
        return;
    }
    let Some((_, host @ Host::Local { .. })) = servers().into_iter().next() else {
        eprintln!("SKIPPED: the flaky proxy runs in front of the local server only");
        return;
    };
    let Host::Local { base, .. } = &host else {
        unreachable!()
    };
    let h = harness(&host, "stale");
    let e = h.engine();
    let Some(proxy) = FlakyProxy::start(
        base,
        &["--stale-after", "1.5", "--log-requests"],
        h.scratch_dir().join("proxy.log"),
    ) else {
        eprintln!("SKIPPED: no server certificate next to the CA for the flaky proxy");
        return;
    };
    let url = format!("{}/{}", proxy.url(base), h.scratch.name);
    let pause = || tokio::time::sleep(std::time::Duration::from_secs(2));
    let info = e
        .checkout(
            &CheckoutPlan {
                url: format!("{url}/trunk"),
                path: h.work_dir().join("wc"),
                rev: None,
                depth: Depth::Infinity,
                sparse: None,
                format: WcFormat::V31,
                store_pristine: true,
                ignore_externals: false,
            },
            &ctx(),
        )
        .await
        .expect("checkout");
    let wc = WcRef {
        id: WcId("wc".to_owned()),
        root: PathBuf::from(info.root_path),
    };
    pause().await;
    h.write_file(&wc.root, "README.md", "after a pause\n")
        .unwrap();
    e.commit(
        &wc,
        &CommitRequest {
            wc_id: wc.id.clone(),
            paths: vec!["README.md".into()],
            message: "after a pause".into(),
            keep_locks: false,
            auto_add_remove: true,
        },
        &ctx(),
    )
    .await
    .expect("commit after a pause");
    pause().await;
    let scope = opensvn_lib::engine::LockScope::Wc {
        wc: wc.clone(),
        paths: vec!["README.md".into()],
    };
    e.lock(&scope, Some("after a pause"), false, &ctx())
        .await
        .expect("lock after a pause");
    pause().await;
    e.unlock(&scope, false, &ctx())
        .await
        .expect("unlock after a pause");
    pause().await;
    e.copy_location(
        &CopyLocationRequest {
            from_url: format!("{url}/trunk"),
            from_rev: None,
            to_url: format!("{url}/branches/after-a-pause"),
            message: "copy after a pause".into(),
            switch_wc_id: None,
            from_wc: None,
        },
        None,
        &ctx(),
    )
    .await
    .expect("copy after a pause");

    let log = std::fs::read_to_string(&proxy.log).unwrap_or_default();
    let resets: Vec<&str> = log.lines().filter(|l| l.contains("idle")).collect();
    assert!(
        !resets.is_empty(),
        "the proxy dropped no connection:\n{log}"
    );
    for r in &resets {
        assert!(
            !WRITE_METHODS.iter().any(|m| r.contains(&format!("({m})"))),
            "a write met a dropped connection: {r}"
        );
    }
    let count = |m: &str| requests(&proxy).iter().filter(|(x, _)| x == m).count();
    // One commit and one copy (HTTP v2: POST, then MERGE), one lock, one unlock.
    assert_eq!(count("MERGE"), 2, "{log}");
    assert_eq!(count("LOCK"), 1, "{log}");
    assert_eq!(count("UNLOCK"), 1, "{log}");
    assert_eq!(count("PUT"), 1, "{log}");
    assert_no_prompt();
}

/// A proxy that closes every reused connection under its next request: a write that goes out
/// on such a connection fails, but is never sent a second time.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn and scripts/test-env/visualsvn-like.sh certificates"]
async fn a_write_the_proxy_cuts_is_never_sent_again() {
    if behind_a_faulty_proxy() {
        eprintln!("SKIPPED: starts its own flaky proxy, so it runs against the server directly");
        return;
    }
    let Some((_, host @ Host::Local { .. })) = servers().into_iter().next() else {
        eprintln!("SKIPPED: the flaky proxy runs in front of the local server only");
        return;
    };
    let Host::Local { base, .. } = &host else {
        unreachable!()
    };
    let h = harness(&host, "cutwrite");
    let e = h.engine();
    let Some(proxy) = FlakyProxy::start(
        base,
        &["--close-after", "1", "--log-requests"],
        h.scratch_dir().join("proxy.log"),
    ) else {
        eprintln!("SKIPPED: no server certificate next to the CA for the flaky proxy");
        return;
    };
    let url = format!("{}/{}", proxy.url(base), h.scratch.name);
    let info = e
        .checkout(
            &CheckoutPlan {
                url: format!("{url}/trunk"),
                path: h.work_dir().join("wc"),
                rev: None,
                depth: Depth::Infinity,
                sparse: None,
                format: WcFormat::V31,
                store_pristine: true,
                ignore_externals: false,
            },
            &ctx(),
        )
        .await
        .expect("checkout (reads are sent again)");
    let wc = WcRef {
        id: WcId("wc".to_owned()),
        root: PathBuf::from(info.root_path),
    };
    for f in ["README.md", "src/main.rs"] {
        h.write_file(&wc.root, f, "changed\n").unwrap();
    }
    // Two PUTs back to back: the second one reuses the connection of the first.
    let outcome = e
        .commit(
            &wc,
            &CommitRequest {
                wc_id: wc.id.clone(),
                paths: vec!["README.md".into(), "src/main.rs".into()],
                message: "cut".into(),
                keep_locks: false,
                auto_add_remove: true,
            },
            &ctx(),
        )
        .await;
    let log = std::fs::read_to_string(&proxy.log).unwrap_or_default();
    let cut_writes: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("closing before request"))
        .filter(|l| WRITE_METHODS.iter().any(|m| l.contains(&format!("({m},"))))
        .collect();
    assert!(
        !cut_writes.is_empty(),
        "the proxy cut no write ({outcome:?}):\n{log}"
    );
    let err = outcome.expect_err("a commit whose write was cut fails");
    assert!(
        err.code == ErrorCode::CommitOutcomeUnknown || err.connection_failure().is_some(),
        "{err:?}"
    );
    let mut writes: Vec<_> = requests(&proxy)
        .into_iter()
        .filter(|(m, _)| WRITE_METHODS.contains(&m.as_str()))
        .collect();
    let all = writes.len();
    writes.sort();
    writes.dedup();
    assert_eq!(writes.len(), all, "a write was sent twice:\n{log}");
    assert_no_prompt();
}

struct NoJournal;

impl svn_ra_api::CommitObserver for NoJournal {
    fn on_txn_created(&self, _txn: &str) -> svn_types::Result<()> {
        Ok(())
    }
    fn on_merged(&self, _info: &svn_ra_api::CommitInfo) -> svn_types::Result<()> {
        Ok(())
    }
}

/// A pause between two writes of one operation (two files of a commit, the commit's MERGE, a
/// lock and its unlock) longer than a proxy keeps idle connections: every write still reaches
/// the server once. Over a session, so no read comes first to flush the dropped connections
/// out of the pool, as the engine's operations do.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn and scripts/test-env/visualsvn-like.sh certificates"]
async fn writes_of_one_commit_survive_a_pause_longer_than_the_proxy_keeps_idle_connections() {
    use md5::{Digest as _, Md5};
    use opensvn_lib::engine::real::SessionConnector as _;
    use svn_types::{Checksum, RepoUrl, Revnum};

    if behind_a_faulty_proxy() {
        eprintln!("SKIPPED: starts its own flaky proxy, so it runs against the server directly");
        return;
    }
    let Some((_, host @ Host::Local { .. })) = servers().into_iter().next() else {
        eprintln!("SKIPPED: the flaky proxy runs in front of the local server only");
        return;
    };
    let Host::Local { base, .. } = &host else {
        unreachable!()
    };
    let h = harness(&host, "pausedcommit");
    let Some(proxy) = FlakyProxy::start(
        base,
        &["--stale-after", "1.5", "--log-requests"],
        h.scratch_dir().join("proxy.log"),
    ) else {
        eprintln!("SKIPPED: no server certificate next to the CA for the flaky proxy");
        return;
    };
    let url = format!("{}/{}", proxy.url(base), h.scratch.name);
    let cancel = svn_types::CancelToken::new();
    let session = connector()
        .connect_with(
            &RepoUrl::parse(&url).unwrap(),
            &opensvn_lib::engine::real::ConnectOptions::at_url(&ctx()),
            &cancel,
        )
        .await
        .expect("session");
    let pause = || tokio::time::sleep(std::time::Duration::from_secs(2));
    let p = |s: &str| RelPath::new(s).unwrap();
    let dir = h.scratch_dir();
    let source = |name: &str, text: &[u8]| {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        let md5: [u8; 16] = Md5::digest(text).into();
        (
            svn_ra_api::ReplayableSource {
                path,
                translated: true,
                size: Some(text.len() as u64),
            },
            Checksum::Md5(md5),
        )
    };

    let mut ed = session
        .commit_editor("paused", &[], &[], false, Arc::new(NoJournal), &cancel)
        .await
        .expect("commit editor");
    let root = ed.open_root(Some(Revnum(1))).await.unwrap();
    let trunk = ed
        .open_directory(&p("trunk"), root, Some(Revnum(1)))
        .await
        .unwrap();
    for name in ["one.txt", "two.txt"] {
        let (src, md5) = source(name, format!("{name}\n").as_bytes());
        let f = ed
            .add_file(&p(&format!("trunk/{name}")), trunk, None)
            .await
            .unwrap();
        ed.apply_text(f, None, src, None, md5).await.unwrap();
        ed.close_file(f)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        pause().await;
    }
    ed.close_directory(trunk).await.unwrap();
    ed.close_directory(root).await.unwrap();
    let info = ed
        .close_edit()
        .await
        .expect("the second PUT and the MERGE after pauses");
    assert_eq!(info.revision, Revnum(2));
    pause().await;
    let lock = session
        .lock(&p("trunk/one.txt"), Some("paused"), false, None, &cancel)
        .await
        .expect("LOCK after a pause");
    pause().await;
    session
        .unlock(&p("trunk/one.txt"), Some(&lock.token), false, &cancel)
        .await
        .expect("UNLOCK after a pause");

    let log = std::fs::read_to_string(&proxy.log).unwrap_or_default();
    for l in log.lines().filter(|l| l.contains("idle")) {
        assert!(
            !WRITE_METHODS.iter().any(|m| l.contains(&format!("({m})"))),
            "a write met a dropped connection: {l}"
        );
    }
    let count = |m: &str| requests(&proxy).iter().filter(|(x, _)| x == m).count();
    for (m, n) in [
        ("POST", 1),
        ("PUT", 2),
        ("MERGE", 1),
        ("LOCK", 1),
        ("UNLOCK", 1),
    ] {
        assert_eq!(count(m), n, "{m}:\n{log}");
    }
    assert_no_prompt();
}

// ---- a switch whose answer is cut off ----

/// Every BASE node of the working copy at `root` is where its place in the tree puts it below
/// `prefix` (nothing left switched, nothing incomplete).
fn assert_all_below(root: &Path, prefix: &str) {
    let db = WcDb::open(root).unwrap();
    let rows: Vec<(String, String, String)> = db
        .with_read(|c| {
            let mut stmt = c
                .prepare(
                    "SELECT local_relpath, COALESCE(repos_path, ''), presence FROM nodes \
                     WHERE op_depth = 0 ORDER BY local_relpath",
                )
                .map_err(svn_wc::db::sql_err)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(svn_wc::db::sql_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(svn_wc::db::sql_err)?;
            Ok(rows)
        })
        .unwrap();
    for (local, repos, presence) in &rows {
        let expected = if local.is_empty() {
            prefix.to_owned()
        } else {
            format!("{prefix}/{local}")
        };
        assert_eq!(repos, &expected, "{local}: {rows:?}");
        assert_eq!(presence, "normal", "{local}: {rows:?}");
    }
}

/// Status entries other than plain unmodified ones.
async fn changes(e: &dyn EngineFacade, wc: &WcRef) -> Vec<opensvn_ipc::FileStatus> {
    e.status(
        wc,
        StatusQuery {
            include_ignored: false,
            include_unversioned: true,
        },
        &ctx(),
    )
    .await
    .expect("status")
    .into_iter()
    .filter(|s| {
        s.text != opensvn_ipc::NodeStatus::Normal
            || !matches!(
                s.props,
                opensvn_ipc::PropStatus::None | opensvn_ipc::PropStatus::Normal
            )
    })
    .collect()
}

/// A proxy that starts `faults` on a port the working copy already uses (the previous proxy
/// there was dropped).
fn proxy_on(base: &str, port: u16, faults: &[&str], log: PathBuf) -> FlakyProxy {
    (0..50)
        .find_map(|_| {
            let p = FlakyProxy::start_on(base, port, faults, log.clone());
            if p.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            p
        })
        .expect("a proxy on the same port")
}

/// A working copy of `trunk` checked out through a plain proxy, with a local edit, and a
/// branch `branches/sw` whose folders hold `files` more files than trunk. Returns the
/// harness pieces the switch tests need.
async fn switch_setup(
    host: &Host,
    base: &str,
    tag: &str,
    files: Vec<(String, Vec<u8>)>,
) -> Option<(DavHarness, WcRef, u16)> {
    let h = harness(host, tag);
    let e = h.engine();
    e.copy_location(
        &CopyLocationRequest {
            from_url: format!("{}/trunk", h.repo_url()),
            from_rev: None,
            to_url: format!("{}/branches/sw", h.repo_url()),
            message: "branch".into(),
            switch_wc_id: None,
            from_wc: None,
        },
        None,
        &ctx(),
    )
    .await
    .expect("branch");
    let mut dirs: Vec<String> = files
        .iter()
        .filter_map(|(p, _)| {
            Path::new(p)
                .parent()
                .map(|d| d.to_string_lossy().into_owned())
        })
        .collect();
    dirs.sort();
    dirs.dedup();
    let dirs: Vec<&str> = dirs.iter().map(String::as_str).collect();
    h.scratch.load(&add_dump(&dirs, &files));
    let healthy = FlakyProxy::start(base, &[], h.scratch_dir().join("proxy-0.log"))?;
    let port = healthy.port;
    let info = e
        .checkout(
            &CheckoutPlan {
                url: format!("{}/{}/trunk", healthy.url(base), h.scratch.name),
                path: h.work_dir().join("wc"),
                rev: None,
                depth: Depth::Infinity,
                sparse: None,
                format: WcFormat::V31,
                store_pristine: true,
                ignore_externals: false,
            },
            &ctx(),
        )
        .await
        .expect("checkout through the proxy");
    let wc = WcRef {
        id: WcId("wc".to_owned()),
        root: PathBuf::from(info.root_path),
    };
    h.write_file(&wc.root, "README.md", "local edit\n").unwrap();
    std::fs::write(wc.root.join("notes.txt"), "unversioned\n").unwrap();
    drop(healthy);
    Some((h, wc, port))
}

/// Only the local edits of [`switch_setup`] differ after a switch.
fn assert_only_local_edits(changed: &[opensvn_ipc::FileStatus]) {
    let mut paths: Vec<&str> = changed.iter().map(|s| s.path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(paths, ["README.md", "notes.txt"], "{changed:?}");
}

/// A proxy that cuts every `REPORT` answer after 128 KiB: the switch's answer (send-all, the
/// texts inline) is cut, and every resumed drive gets further, since it asks only for what the
/// broken drives did not commit. The switch finishes, every node ends up below the branch,
/// and the local edits are kept.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn and scripts/test-env/visualsvn-like.sh certificates"]
async fn switch_cut_mid_answer_resumes_until_done() {
    const DIRS: usize = 6;
    const FILES: usize = 12;
    const SIZE: usize = 8192;
    if behind_a_faulty_proxy() {
        eprintln!("SKIPPED: starts its own flaky proxy, so it runs against the server directly");
        return;
    }
    let Some((_, host @ Host::Local { .. })) = servers().into_iter().next() else {
        eprintln!("SKIPPED: the flaky proxy runs in front of the local server only");
        return;
    };
    let Host::Local { base, .. } = &host else {
        unreachable!()
    };
    let mut files = Vec::new();
    for d in 0..DIRS {
        for f in 0..FILES {
            files.push((format!("branches/sw/d{d}/f{f:02}.bin"), noise(SIZE)));
        }
    }
    let Some((h, wc, port)) = switch_setup(&host, base, "switchcut", files).await else {
        eprintln!("SKIPPED: no server certificate next to the CA for the flaky proxy");
        return;
    };
    let e = h.engine();
    let cutting = proxy_on(
        base,
        port,
        &[
            "--truncate-every",
            "1",
            "--truncate-at",
            "131072",
            "--truncate-method",
            "REPORT",
        ],
        h.scratch_dir().join("proxy-1.log"),
    );
    let target = format!("{}/{}/branches/sw", cutting.url(base), h.scratch.name);
    let reached = e
        .switch(&wc, &target, None, false, &ctx())
        .await
        .unwrap_or_else(|err| {
            panic!(
                "switch through a cutting proxy: {err:?}\n{}",
                std::fs::read_to_string(&cutting.log).unwrap_or_default()
            )
        });
    assert!(
        cutting.cuts() >= 2,
        "{}",
        std::fs::read_to_string(&cutting.log).unwrap_or_default()
    );
    let info = e.wc_info(&wc.root, &ctx()).await.expect("info");
    assert_eq!(info.base_rev, reached);
    assert!(info.url.ends_with("/branches/sw"), "{}", info.url);
    assert!(!info.incomplete);
    assert_all_below(&wc.root, "branches/sw");
    for d in 0..DIRS {
        assert_eq!(count_files(&wc.root.join(format!("d{d}"))), FILES, "d{d}");
    }
    assert_eq!(
        std::fs::read_to_string(wc.root.join("README.md")).unwrap(),
        "local edit\n"
    );
    assert_only_local_edits(&changes(e, &wc).await);
    assert_no_prompt();
}

/// A proxy that always cuts the switch's answer at the same offset, inside a file bigger than
/// the offset: the switch gives up after two resumes that commit nothing new and leaves the
/// working copy half-switched but consistent (the root at the branch and incomplete, local
/// edits untouched, nothing queued). Once the connection works, the same switch (what
/// "Continue switch" runs) finishes it: every node below the branch.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn and scripts/test-env/visualsvn-like.sh certificates"]
async fn switch_cut_at_the_same_offset_is_finished_by_the_same_switch() {
    const BIG: usize = 2 << 20;
    if behind_a_faulty_proxy() {
        eprintln!("SKIPPED: starts its own flaky proxy, so it runs against the server directly");
        return;
    }
    let Some((_, host @ Host::Local { .. })) = servers().into_iter().next() else {
        eprintln!("SKIPPED: the flaky proxy runs in front of the local server only");
        return;
    };
    let Host::Local { base, .. } = &host else {
        unreachable!()
    };
    let mut files = Vec::new();
    for f in 0..8 {
        files.push((format!("branches/sw/aaa/s{f}.txt"), vec![b'x'; 2048]));
    }
    files.push(("branches/sw/zzz/z-big.bin".to_owned(), noise(BIG)));
    let Some((h, wc, port)) = switch_setup(&host, base, "switchstall", files).await else {
        eprintln!("SKIPPED: no server certificate next to the CA for the flaky proxy");
        return;
    };
    let e = h.engine();
    let cutting = proxy_on(
        base,
        port,
        &[
            "--truncate-every",
            "1",
            "--truncate-at",
            "262144",
            "--truncate-method",
            "REPORT,GET",
        ],
        h.scratch_dir().join("proxy-1.log"),
    );
    let target = format!("{}/{}/branches/sw", cutting.url(base), h.scratch.name);
    let err = e
        .switch(&wc, &target, None, false, &ctx())
        .await
        .expect_err("a switch that never gets past the cut");
    assert!(
        err.connection_failure().is_some(),
        "connection failure expected: {err:?}"
    );
    // The first drive, then two resumes that committed nothing new.
    assert_eq!(
        cutting.cuts(),
        3,
        "{}",
        std::fs::read_to_string(&cutting.log).unwrap_or_default()
    );
    drop(cutting);
    let info = e.wc_info(&wc.root, &ctx()).await.expect("info");
    assert!(info.url.ends_with("/branches/sw"), "{}", info.url);
    assert!(info.incomplete, "a half-switched working copy is resumable");
    assert!(!wc.root.join("zzz/z-big.bin").exists(), "no partial file");
    assert_eq!(
        std::fs::read_to_string(wc.root.join("README.md")).unwrap(),
        "local edit\n"
    );
    assert_eq!(
        std::fs::read_to_string(wc.root.join("notes.txt")).unwrap(),
        "unversioned\n"
    );

    let _healthy = proxy_on(base, port, &[], h.scratch_dir().join("proxy-2.log"));
    e.switch(&wc, &target, info.resume_rev, false, &ctx())
        .await
        .expect("the same switch finishes");
    assert_all_below(&wc.root, "branches/sw");
    assert_eq!(
        std::fs::metadata(wc.root.join("zzz/z-big.bin"))
            .unwrap()
            .len(),
        BIG as u64
    );
    assert_eq!(count_files(&wc.root.join("aaa")), 8);
    assert_only_local_edits(&changes(e, &wc).await);
    assert_no_prompt();
}
