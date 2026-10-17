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
use opensvn_ipc::{
    CommitRequest, CopyLocationRequest, Depth, SparseDepth, SparseEntry, UpdateRequest, WcFormat,
    WcId,
};
use opensvn_lib::engine::conformance::{self, Harness, PlantedOwner};
use opensvn_lib::engine::real::{ClientRecovery, HttpConnector, RealEngine, RealEngineConfig};
use opensvn_lib::engine::{CheckoutPlan, CompareLeft, EngineFacade, OpContext, StatusQuery, WcRef};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject as _;
use svn_ra::transport::{
    DEFAULT_WRITE_CONNECTION_REQUESTS, DEFAULT_WRITE_REUSE_WINDOW, MemoryTrustStore, ProxyConfig,
};
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
// resumed). Writes must succeed too when the faults are ones the transport keeps writes away
// from ([`writes_survive`]: a connection reset or closed after two or more requests, or dropped
// after an idle time longer than the write reuse window): then no write may meet a fault at
// all. Under any other fault, a write whose connection the proxy breaks is never sent again
// (#166): it fails, and the suite checks that it failed for that reason and left nothing behind,
// then repeats it like a user would (`write_op!`). The tests that start their own proxy are
// skipped.

fn behind_a_faulty_proxy() -> bool {
    std::env::var("OPENSVN_COMPAT_FAULTS").is_ok_and(|f| !f.trim().is_empty())
}

/// Whether every write gets through the proxy faults `faults` (`OPENSVN_COMPAT_FAULTS`)
/// untouched: the transport sends a write on a connection's first or second request only
/// ([`DEFAULT_WRITE_CONNECTION_REQUESTS`]) and never on one idle for longer than
/// [`DEFAULT_WRITE_REUSE_WINDOW`], and the settling writes on a new connection. Cutting
/// responses of writes, outages and limits below two requests per connection break writes.
fn writes_survive(faults: &str) -> bool {
    let window = DEFAULT_WRITE_REUSE_WINDOW.as_secs_f64();
    let mut args = faults.split_whitespace();
    let mut cut_methods: Option<Vec<String>> = None;
    let mut cuts = false;
    while let Some(flag) = args.next() {
        let mut value = || args.next().unwrap_or_default().to_owned();
        match flag {
            "--log-requests" => {}
            "--truncate-at" => {
                value();
            }
            "--close-after" | "--reset-after" => {
                if value().parse::<usize>().unwrap_or(0) < DEFAULT_WRITE_CONNECTION_REQUESTS {
                    return false;
                }
            }
            "--idle-reset" | "--stale-after" => {
                if value().parse::<f64>().unwrap_or(0.0) <= window {
                    return false;
                }
            }
            "--truncate-every" => cuts = value().parse::<u32>().unwrap_or(0) > 0,
            "--truncate-method" => {
                cut_methods = Some(
                    value()
                        .split(',')
                        .map(|m| m.trim().to_ascii_uppercase())
                        .collect(),
                );
            }
            _ => return false,
        }
    }
    !cuts || cut_methods.is_some_and(|ms| ms.iter().all(|m| !WRITE_METHODS.contains(&m.as_str())))
}

/// The proxy faults that hit a write of this repository after line `mark` of the proxy log: a
/// reset or close under a write request, or a cut write response.
fn writes_hit_by_the_proxy(scratch: &Scratch, mark: usize) -> Vec<String> {
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
    // "closing before request 3 (PUT, no close_notify)", "resetting before request 3 (PUT)",
    // "resetting a connection idle 2.1s under request 3 (PUT)", "truncated a PUT response".
    lines
        .iter()
        .filter(|l| !l.contains(": request "))
        .filter(|l| conn_of(l).is_some_and(|c| ours.contains(&c)))
        .filter(|l| {
            WRITE_METHODS.iter().any(|m| {
                l.contains(&format!("({m})"))
                    || l.contains(&format!("({m},"))
                    || l.contains(&format!("truncated a {m} response"))
            })
        })
        .map(|l| (*l).to_owned())
        .collect()
}

/// The proxy cuts answers: the svn command line client, which the comparisons with svn run
/// through the same URL, does not resume a cut answer (E120106), so they cannot run.
fn proxy_cuts_answers() -> bool {
    std::env::var("OPENSVN_COMPAT_FAULTS").is_ok_and(|f| f.contains("--truncate"))
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
            && !writes_survive(&std::env::var("OPENSVN_COMPAT_FAULTS").unwrap_or_default())
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
                let h = harness(&host, stringify!($name));
                let mark = h.write_mark();
                conformance::$name(&h).await;
                // Faults the writes are kept away from: none may have met one.
                if behind_a_faulty_proxy() && !h.writes_may_break() {
                    let hit = writes_hit_by_the_proxy(&h.scratch, mark);
                    assert!(hit.is_empty(), "writes met a proxy fault: {hit:#?}");
                }
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
scenario!(branch_detection_follows_a_switch);
scenario!(repository_content_tools);
scenario!(open_recovers_dead_locks_and_reports_foreign_ones);
scenario!(locked_unmodified_file_in_status);
scenario!(needs_lock_unmodified_file_in_status);
scenario!(lock_held_elsewhere_and_steal);
scenario!(commit_with_auto_add_remove);
scenario!(update_ends_in_conflict);
scenario!(update_of_sub_paths);

scenario!(mark_move_and_set_depth);
scenario!(sparse_checkout_and_choose_folders);

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

    let loc = e
        .node_location(&b, "", &ctx())
        .await
        .expect("location of the working copy root");
    assert_eq!(loc.repos_path, "trunk");
    let log = e
        .log(
            &opensvn_lib::engine::LogScope::Repo(loc),
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
                include_merged: None,
                revprops: None,
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

/// A proxy that closes every reused connection under its next request (below the two requests a
/// write connection may carry): a write that goes out on such a connection fails, is never sent
/// a second time, and once the server confirmed the transaction deleted the commit reports that
/// nothing was committed.
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
    assert_eq!(err.code, ErrorCode::CommitNotApplied, "{err:?}");
    assert!(err.connection_failure().is_some(), "{err:?}");
    assert!(h.scratch.transactions().is_empty());
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

/// Versioned and unversioned nodes.
const ALL_FILES: StatusQuery = StatusQuery {
    include_ignored: false,
    include_unversioned: true,
};

/// Checks out `trunk` of the harness's repository through `url` (the proxy's) into `name`.
async fn checkout_via(h: &DavHarness, url: &str, name: &str) -> WcRef {
    let info = h
        .engine()
        .checkout(
            &CheckoutPlan {
                url: format!("{url}/trunk"),
                path: h.work_dir().join(name),
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
    WcRef {
        id: WcId(name.to_owned()),
        root: PathBuf::from(info.root_path),
    }
}

/// Commits `paths` of `wc` with `message`.
async fn commit_paths(
    h: &DavHarness,
    wc: &WcRef,
    paths: &[String],
    message: &str,
) -> Result<opensvn_ipc::Rev, SvnError> {
    h.engine()
        .commit(
            wc,
            &CommitRequest {
                wc_id: wc.id.clone(),
                paths: paths.to_vec(),
                message: message.into(),
                keep_locks: false,
                auto_add_remove: true,
            },
            &ctx(),
        )
        .await
}

/// A middlebox that resets every connection under its third request (`--reset-after 2`): a
/// commit of 60 files, a lock, an unlock and a copy all get through, every write is sent once,
/// and no write ever meets the reset (only reads do, and they are sent again).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn and scripts/test-env/visualsvn-like.sh certificates"]
async fn a_commit_gets_through_a_proxy_that_resets_connections_after_two_requests() {
    if behind_a_faulty_proxy() {
        eprintln!("SKIPPED: starts its own flaky proxy, so it runs against the server directly");
        return;
    }
    for (version, host) in servers() {
        let Host::Local { base, .. } = &host else {
            eprintln!("SKIPPED: the flaky proxy runs in front of the local server only");
            return;
        };
        eprintln!("server {version}");
        let h = harness(&host, "reset2");
        let Some(proxy) = FlakyProxy::start(
            base,
            &["--reset-after", "2", "--log-requests"],
            h.scratch_dir().join("proxy.log"),
        ) else {
            eprintln!("SKIPPED: no server certificate next to the CA for the flaky proxy");
            return;
        };
        let url = format!("{}/{}", proxy.url(base), h.scratch.name);
        let wc = checkout_via(&h, &url, "wc").await;
        let mut paths = vec!["README.md".to_owned()];
        h.write_file(&wc.root, "README.md", "changed\n").unwrap();
        for i in 0..60 {
            let p = format!("many/f{i:02}.txt");
            h.write_file(&wc.root, &p, &format!("file {i}\n")).unwrap();
            paths.push(p);
        }
        commit_paths(&h, &wc, &paths, "through a resetting proxy")
            .await
            .expect("commit through a proxy that resets after two requests");
        let scope = opensvn_lib::engine::LockScope::Wc {
            wc: wc.clone(),
            paths: vec!["README.md".into()],
        };
        let e = h.engine();
        e.lock(&scope, Some("reset proxy"), false, &ctx())
            .await
            .expect("lock");
        e.unlock(&scope, false, &ctx()).await.expect("unlock");
        e.copy_location(
            &CopyLocationRequest {
                from_url: format!("{url}/trunk"),
                from_rev: None,
                to_url: format!("{url}/branches/reset2"),
                message: "copy through a resetting proxy".into(),
                switch_wc_id: None,
                from_wc: None,
            },
            None,
            &ctx(),
        )
        .await
        .expect("copy");

        let log = std::fs::read_to_string(&proxy.log).unwrap_or_default();
        let resets: Vec<&str> = log
            .lines()
            .filter(|l| l.contains("resetting before request"))
            .collect();
        for r in &resets {
            assert!(
                !WRITE_METHODS.iter().any(|m| r.contains(&format!("({m})"))),
                "a write met the reset: {r}\n{log}"
            );
        }
        let count = |m: &str| requests(&proxy).iter().filter(|(x, _)| x == m).count();
        assert_eq!(count("PUT"), 61, "{log}");
        assert_eq!(count("MERGE"), 2, "{log}");
        assert_eq!(count("LOCK"), 1, "{log}");
        assert_eq!(count("UNLOCK"), 1, "{log}");
        let mut writes: Vec<_> = requests(&proxy)
            .into_iter()
            .filter(|(m, _)| WRITE_METHODS.contains(&m.as_str()))
            .filter(|(m, _)| m != "POST" && m != "MERGE")
            .collect();
        let all = writes.len();
        writes.sort();
        writes.dedup();
        assert_eq!(writes.len(), all, "a write was sent twice:\n{log}");
        assert!(h.scratch.transactions().is_empty());
        // The working copy is at the commit.
        let st = e.status(&wc, ALL_FILES, &ctx()).await.expect("status");
        assert!(
            !st.iter().any(|f| matches!(
                f.text,
                opensvn_ipc::NodeStatus::Modified
                    | opensvn_ipc::NodeStatus::Added
                    | opensvn_ipc::NodeStatus::Unversioned
            )),
            "{st:?}"
        );
    }
    assert_no_prompt();
}

/// A `PUT` whose response the proxy cuts off: the commit stops, the transaction is deleted,
/// and the error says nothing was committed (`commit-not-applied`, Retry). The same commit
/// then goes through once the connection behaves.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn and scripts/test-env/visualsvn-like.sh certificates"]
async fn a_cut_put_commits_nothing_and_the_same_commit_can_be_sent_again() {
    if behind_a_faulty_proxy() {
        eprintln!("SKIPPED: starts its own flaky proxy, so it runs against the server directly");
        return;
    }
    for (version, host) in servers() {
        let Host::Local { base, .. } = &host else {
            eprintln!("SKIPPED: the flaky proxy runs in front of the local server only");
            return;
        };
        eprintln!("server {version}");
        let h = harness(&host, "cutput");
        let Some(proxy) = FlakyProxy::start(
            base,
            &[
                "--truncate-every",
                "2",
                "--truncate-method",
                "PUT",
                "--truncate-at",
                "1",
                "--log-requests",
            ],
            h.scratch_dir().join("proxy.log"),
        ) else {
            eprintln!("SKIPPED: no server certificate next to the CA for the flaky proxy");
            return;
        };
        let port = proxy.port;
        let url = format!("{}/{}", proxy.url(base), h.scratch.name);
        let wc = checkout_via(&h, &url, "wc").await;
        let paths: Vec<String> = ["README.md", "src/main.rs"].map(str::to_owned).to_vec();
        for p in &paths {
            h.write_file(&wc.root, p, "changed\n").unwrap();
        }
        let before = h
            .engine()
            .status(&wc, ALL_FILES, &ctx())
            .await
            .expect("status");
        let err = commit_paths(&h, &wc, &paths, "cut")
            .await
            .expect_err("the second PUT is cut");
        assert_eq!(proxy.cuts(), 1, "{err:?}");
        assert_eq!(err.code, ErrorCode::CommitNotApplied, "{err:?}");
        assert!(err.connection_failure().is_some(), "{err:?}");
        let app = opensvn_ipc::AppError::from(&err);
        assert_eq!(app.code, opensvn_ipc::ErrorCode::CommitNotApplied);
        assert!(app.retryable);
        assert_eq!(
            app.actions,
            Some(Box::new(vec![opensvn_ipc::ErrorAction::Retry]))
        );
        // Nothing committed, nothing left on the server, the working copy as it was.
        assert!(h.scratch.transactions().is_empty());
        assert!(requests(&proxy).iter().all(|(m, _)| m != "MERGE"));
        let after = h
            .engine()
            .status(&wc, ALL_FILES, &ctx())
            .await
            .expect("status");
        assert_eq!(format!("{after:?}"), format!("{before:?}"));

        // The connection behaves again: the very same commit goes through.
        drop(proxy);
        let plain = proxy_on(
            base,
            port,
            &["--log-requests"],
            h.scratch_dir().join("plain.log"),
        );
        let rev = commit_paths(&h, &wc, &paths, "cut")
            .await
            .expect("the same commit, sent again");
        assert_eq!(rev, opensvn_ipc::Rev(2));
        assert_eq!(
            requests(&plain).iter().filter(|(m, _)| m == "PUT").count(),
            2
        );
        drop(plain);
    }
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

// ---- sparse checkout against svn 1.15 over the same server ----

/// NODES rows of a working copy, without the parts that differ between two checkouts of one
/// repository (the working copy id and the last-modified times).
fn nodes_rows(wc: &Path) -> Vec<String> {
    let db = WcDb::open(wc).unwrap();
    db.with_read(|c| {
        let mut stmt = c
            .prepare(
                "SELECT local_relpath, op_depth, presence, kind, revision, repos_path, checksum, \
                 depth, translated_size, changed_revision, changed_author FROM nodes \
                 ORDER BY local_relpath, op_depth",
            )
            .map_err(svn_wc::db::sql_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(format!(
                    "{}|{}|{}|{}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    r.get::<_, Option<i64>>(9)?,
                    r.get::<_, Option<String>>(10)?,
                ))
            })
            .map_err(svn_wc::db::sql_err)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(svn_wc::db::sql_err)
    })
    .unwrap()
}

/// `svn` against the test server: the test CA trusted, the fixed account, no cached
/// credentials.
fn svn_cli(oracle: &opensvn_svn_oracle::Oracle, cwd: &Path, args: &[&str]) {
    let mut c = oracle.svn();
    c.current_dir(cwd)
        .args([
            "--username",
            "tester",
            "--password",
            "tester-pass",
            "--no-auth-cache",
            "--config-option",
        ])
        .arg(format!(
            "servers:global:ssl-authority-files={}",
            ca_path().display()
        ))
        .args(args);
    run(c, None);
}

/// [`svn_cli`] that returns what svn printed.
fn svn_cli_out(oracle: &opensvn_svn_oracle::Oracle, cwd: &Path, args: &[&str]) -> String {
    let mut c = oracle.svn();
    c.current_dir(cwd)
        .args([
            "--username",
            "tester",
            "--password",
            "tester-pass",
            "--no-auth-cache",
            "--config-option",
        ])
        .arg(format!(
            "servers:global:ssl-authority-files={}",
            ca_path().display()
        ))
        .args(args);
    let out = c.output().unwrap();
    assert!(
        out.status.success(),
        "{c:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// `(path, item, props, kind)` rows of `svn diff --summarize --xml`, relative to `base` (a URL,
/// or `.` for a working copy), sorted; the root is left out when `skip_root`.
fn summary_rows(xml: &str, base: &str, skip_root: bool) -> Vec<(String, String, String, String)> {
    let attr = |tag: &str, name: &str| {
        let key = format!("{name}=\"");
        let start = tag.find(&key).unwrap() + key.len();
        tag[start..start + tag[start..].find('"').unwrap()].to_owned()
    };
    let mut rows: Vec<_> = xml
        .split("<path")
        .skip(1)
        .filter(|c| c.starts_with(char::is_whitespace))
        .map(|chunk| {
            let close = chunk.find('>').unwrap();
            let (tag, text) = (
                &chunk[..close],
                &chunk[close + 1..chunk.find("</path>").unwrap()],
            );
            let path = if text == base || text == "." {
                String::new()
            } else {
                text.strip_prefix(&format!("{base}/"))
                    .unwrap_or(text)
                    .to_owned()
            };
            (
                path,
                attr(tag, "item"),
                attr(tag, "props"),
                attr(tag, "kind"),
            )
        })
        .filter(|r| !(skip_root && r.0.is_empty()))
        .collect();
    rows.sort();
    rows
}

fn engine_rows(
    s: &opensvn_ipc::CompareSummary,
    skip_root: bool,
) -> Vec<(String, String, String, String)> {
    use opensvn_ipc::{CompareChange as C, EntryKind};
    let mut rows: Vec<_> = s
        .entries
        .iter()
        .map(|e| {
            (
                e.path.clone(),
                match e.change {
                    C::Added => "added",
                    C::Deleted => "deleted",
                    C::Modified => "modified",
                    C::Normal => "none",
                }
                .to_owned(),
                if e.props_changed { "modified" } else { "none" }.to_owned(),
                if e.kind == EntryKind::Dir {
                    "dir"
                } else {
                    "file"
                }
                .to_owned(),
            )
        })
        .filter(|r| !(skip_root && r.0.is_empty()))
        .collect();
    rows.sort();
    rows
}

/// Compare with branch (#18 follow-up): the summary of two folders at chosen revisions over
/// DAV, and of a working copy with local changes against a folder, is the one svn 1.15 prints
/// for `svn diff --summarize` against the same server (text, file and folder properties, a
/// deleted folder with what it held, an added folder, a move, a replaced file, nothing at all).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn (see the module docs)"]
async fn compare_summary_matches_svn() {
    let Some(oracle) =
        opensvn_svn_oracle::Oracle::resolve(opensvn_svn_oracle::MinVersion::V1_15).unwrap()
    else {
        return;
    };
    if negotiate_mode() {
        eprintln!("skipping: svn is given Basic credentials");
        return;
    }
    // svn itself builds the history and answers through the same URL: a proxy that cuts
    // answers fails svn (E120106), not the engine. The engine's retry of a cut comparison is
    // covered by `summarize_reconnect` (svn-client).
    if behind_a_faulty_proxy() {
        eprintln!("skipping: svn cannot work through the faulty proxy");
        return;
    }
    for (version, host) in servers() {
        eprintln!("compare_summary_matches_svn: server {version}");
        let h = harness(&host, "compare-svn");
        let url = h.repo_url();
        let work = h.work_dir();
        std::fs::create_dir_all(&work).unwrap();
        // r2: more content on trunk; r3: the branch; r4: edits on trunk; r5: one on the branch.
        svn_cli(
            &oracle,
            &work,
            &["checkout", "-q", &format!("{url}/trunk"), "seed"],
        );
        let seed = work.join("seed");
        for (p, text) in [
            ("d/a", "a\n"),
            ("d/sub/b", "b\n"),
            ("e/c", "c\n"),
            ("e/deep/f", "f\n"),
            ("x", "x\n"),
            ("y", "y\n"),
            ("z", "z\n"),
        ] {
            h.write_file(&seed, p, text).unwrap();
        }
        svn_cli(&oracle, &seed, &["add", "-q", "d", "e", "x", "y", "z"]);
        svn_cli(&oracle, &seed, &["commit", "-q", "-m", "r2"]);
        svn_cli(
            &oracle,
            &work,
            &[
                "copy",
                "-q",
                "-m",
                "r3",
                &format!("{url}/trunk"),
                &format!("{url}/branches/cmp"),
            ],
        );
        svn_cli(&oracle, &seed, &["update", "-q"]);
        h.write_file(&seed, "x", "x\nmore\n").unwrap();
        svn_cli(&oracle, &seed, &["propset", "-q", "p", "v", "y"]);
        svn_cli(&oracle, &seed, &["propset", "-q", "dp", "v", "d"]);
        svn_cli(&oracle, &seed, &["rm", "-q", "e", "z"]);
        h.write_file(&seed, "z", "zz\n").unwrap();
        h.write_file(&seed, "n/m/g", "g\n").unwrap();
        svn_cli(&oracle, &seed, &["add", "-q", "z", "n"]);
        svn_cli(&oracle, &seed, &["mv", "-q", "d/a", "d/a2"]);
        svn_cli(&oracle, &seed, &["commit", "-q", "-m", "r4"]);
        svn_cli(
            &oracle,
            &work,
            &["checkout", "-q", &format!("{url}/branches/cmp"), "br"],
        );
        h.write_file(&work.join("br"), "x", "x\nbranch\n").unwrap();
        svn_cli(&oracle, &work.join("br"), &["commit", "-q", "-m", "r5"]);

        let wc = checkout_trunk(&h, "ours").await;
        // The last column: whether anything differs.
        for (left, left_rev, right, right_rev, differs) in [
            ("branches/cmp", 5, "trunk", None, true),
            ("trunk", 5, "branches/cmp", None, true),
            ("trunk", 2, "trunk", Some(4), true),
            ("trunk", 4, "trunk", Some(3), true),
            ("trunk", 2, "branches/cmp", Some(3), false),
        ] {
            let left_url = format!("{url}/{left}");
            let right_arg = format!(
                "{url}/{right}@{}",
                right_rev.map_or("HEAD".to_owned(), |r: u64| r.to_string())
            );
            let theirs = summary_rows(
                &svn_cli_out(
                    &oracle,
                    &work,
                    &[
                        "diff",
                        "--summarize",
                        "--xml",
                        &format!("{left_url}@{left_rev}"),
                        &right_arg,
                    ],
                ),
                &left_url,
                false,
            );
            let ours = h
                .engine
                .compare_summarize(
                    CompareLeft::Url {
                        url: &left_url,
                        rev: Some(opensvn_ipc::Rev(left_rev)),
                    },
                    &format!("{url}/{right}"),
                    right_rev.map(opensvn_ipc::Rev),
                    &ctx(),
                )
                .await
                .expect("compare");
            assert_eq!(
                engine_rows(&ours, false),
                theirs,
                "{version}: {left}@{left_rev} -> {right}@{right_rev:?}"
            );
            assert_eq!(ours.left_rev, Some(opensvn_ipc::Rev(left_rev)));
            assert_eq!(
                !theirs.is_empty(),
                differs,
                "{version}: {left}@{left_rev} -> {right}"
            );
        }

        // A working copy with local changes, against the branch (svn: `--old=WC --new=URL`;
        // the root left out, see `summarize_wc`).
        svn_cli(
            &oracle,
            &work,
            &["checkout", "-q", &format!("{url}/trunk"), "theirs"],
        );
        let theirs_wc = work.join("theirs");
        for root in [&theirs_wc, &wc.root] {
            h.write_file(root, "y", "y\nlocal\n").unwrap();
            h.write_file(root, "q", "q\n").unwrap();
        }
        svn_cli(&oracle, &theirs_wc, &["add", "-q", "q"]);
        svn_cli(&oracle, &theirs_wc, &["rm", "-q", "d/sub/b"]);
        h.engine
            .add_paths(&wc, &["q".to_owned()], &ctx())
            .await
            .expect("add");
        h.engine
            .delete_paths(&wc, &["d/sub/b".to_owned()], false, &ctx())
            .await
            .expect("delete");
        let theirs = summary_rows(
            &svn_cli_out(
                &oracle,
                &theirs_wc,
                &[
                    "diff",
                    "--summarize",
                    "--xml",
                    "--old=.",
                    &format!("--new={url}/branches/cmp"),
                ],
            ),
            ".",
            true,
        );
        let ours = h
            .engine
            .compare_summarize(
                CompareLeft::Wc(&wc),
                &format!("{url}/branches/cmp"),
                None,
                &ctx(),
            )
            .await
            .expect("compare the working copy");
        assert!(!theirs.is_empty());
        assert_eq!(engine_rows(&ours, true), theirs, "{version}: working copy");
        assert_eq!(ours.left_rev, None);
    }
    assert_no_prompt();
}

/// A checkout with a depth per folder through the real engine leaves the same wc.db as svn
/// 1.15 after `svn checkout --depth` and `svn update --parents --set-depth` per folder against
/// the same server; "Choose folders" afterwards too.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn (see the module docs)"]
async fn sparse_checkout_matches_svn() {
    let Some(oracle) =
        opensvn_svn_oracle::Oracle::resolve(opensvn_svn_oracle::MinVersion::V1_15).unwrap()
    else {
        return;
    };
    if negotiate_mode() {
        eprintln!("skipping: svn is given Basic credentials");
        return;
    }
    if proxy_cuts_answers() {
        eprintln!("SKIPPED: svn cannot resume an answer the proxy cuts (E120106)");
        return;
    }
    let entry = |path: &str, depth: SparseDepth| SparseEntry {
        path: path.to_owned(),
        depth,
    };
    for (version, host) in servers() {
        eprintln!("sparse_checkout_matches_svn: server {version}");
        let h = harness(&host, "sparse-svn");
        let url = h.repo_url();
        let work = h.work_dir();
        std::fs::create_dir_all(&work).unwrap();
        let theirs = work.join("svn");
        svn_cli(
            &oracle,
            &work,
            &["checkout", "-q", "--depth", "empty", &url, "svn"],
        );
        for (depth, path) in [("files", "trunk/src"), ("empty", "branches")] {
            svn_cli(
                &oracle,
                &theirs,
                &["update", "-q", "--parents", "--set-depth", depth, path],
            );
        }
        let ours = work.join("ours");
        h.engine
            .checkout(
                &CheckoutPlan {
                    url: url.clone(),
                    path: ours.clone(),
                    rev: None,
                    depth: Depth::Empty,
                    sparse: Some(vec![
                        entry("branches", SparseDepth::Empty),
                        entry("trunk/src", SparseDepth::Files),
                    ]),
                    format: WcFormat::V31,
                    store_pristine: true,
                    ignore_externals: false,
                },
                &ctx(),
            )
            .await
            .expect("sparse checkout");
        assert_eq!(
            nodes_rows(&ours),
            nodes_rows(&theirs),
            "{version}: checkout"
        );

        let wc = WcRef {
            id: WcId("ours".into()),
            root: ours.clone(),
        };
        h.engine
            .sparse_apply(
                &wc,
                &[
                    entry("trunk", SparseDepth::Infinity),
                    entry("trunk/src", SparseDepth::Exclude),
                    entry("tags", SparseDepth::Empty),
                ],
                &ctx(),
            )
            .await
            .expect("choose folders");
        for (depth, path) in [
            ("infinity", "trunk"),
            ("exclude", "trunk/src"),
            ("empty", "tags"),
        ] {
            svn_cli(
                &oracle,
                &theirs,
                &["update", "-q", "--parents", "--set-depth", depth, path],
            );
        }
        assert_eq!(
            nodes_rows(&ours),
            nodes_rows(&theirs),
            "{version}: choose folders"
        );
        // svn keeps working with the result.
        svn_cli(&oracle, &ours, &["update", "-q"]);
        svn_cli(&oracle, &ours, &["status"]);
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
    // The branch itself exists (the copy above); files may go straight into it.
    dirs.retain(|d| d != "branches/sw");
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

// ---- switch keeps excluded children (svn 1.15) ----

/// BASE rows that do not depend on the client or the repository URL.
fn base_rows(wc: &Path) -> Vec<String> {
    let db = WcDb::open(wc).unwrap();
    db.with_read(|c| {
        let mut s = c
            .prepare(
                "SELECT local_relpath, presence, kind, revision, repos_path, depth FROM nodes \
                 WHERE op_depth = 0 ORDER BY local_relpath",
            )
            .unwrap();
        let rows = s
            .query_map([], |r| {
                Ok(format!(
                    "{}|{}|{}|{:?}|{:?}|{:?}",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            })
            .unwrap();
        Ok(rows.map(|r| r.unwrap()).collect::<Vec<_>>())
    })
    .unwrap()
}

/// An excluded folder stays excluded when the working copy is switched to a branch over
/// mod_dav_svn: its BASE row moves to the branch at the new revision, like `svn switch` leaves
/// it. With a local server and svn 1.15 (`OPENSVN_SVN_BIN`), the rows are compared with svn's
/// own switch of the same repository (over `file://`).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn (see the module docs)"]
async fn switch_keeps_an_excluded_folder_excluded_like_svn() {
    use opensvn_lib::engine::real::SessionConnector as _;
    for (version, host) in servers() {
        eprintln!("server {version}");
        let h = harness(&host, "switch-excluded");
        let url = h.repo_url();
        let e = h.engine();
        let root = h.work_dir().join("excluded");
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
            id: WcId("excluded".into()),
            root: PathBuf::from(info.root_path),
        };
        // `svn update --set-depth exclude src`, through a session at the folder.
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
        svn_client::set_depth(
            &client,
            session.as_ref(),
            &wc.root.join("src"),
            None,
            svn_types::Depth::Exclude,
            &cancel,
        )
        .await
        .expect("exclude src");
        let branched = opensvn_lib::write_op!(
            &h,
            e.copy_location(
                &CopyLocationRequest {
                    from_url: format!("{url}/trunk"),
                    from_rev: None,
                    to_url: format!("{url}/branches/b"),
                    message: "Branch".into(),
                    switch_wc_id: None,
                    from_wc: None,
                },
                None,
                &ctx(),
            )
        )
        .expect("branch");
        let reached = e
            .switch(&wc, &format!("{url}/branches/b"), None, false, &ctx())
            .await
            .expect("switch");
        assert_eq!(reached, branched);
        let rows = base_rows(&wc.root);
        assert!(
            rows.contains(&format!(
                "src|excluded|dir|Some({})|Some(\"branches/b/src\")|None",
                reached.0
            )),
            "{rows:#?}"
        );
        assert!(!wc.root.join("src").exists());

        // The same with svn 1.15 on the same repository.
        let (Host::Local { repos, .. }, Some(oracle)) = (
            &host,
            opensvn_svn_oracle::Oracle::resolve(opensvn_svn_oracle::MinVersion::V1_15)
                .ok()
                .flatten(),
        ) else {
            eprintln!("no local repository or svn 1.15: the svn comparison is skipped");
            continue;
        };
        let file_url = opensvn_svn_oracle::Oracle::file_url(&repos.join(&h.scratch.name));
        let theirs = h.tmp.path().join("svn-wc");
        let svn = |args: &[&str]| {
            let out = oracle.svn().args(args).output().unwrap();
            assert!(
                out.status.success(),
                "svn {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        let t = theirs.to_str().unwrap();
        svn(&["checkout", "-q", &format!("{file_url}/trunk@1"), t]);
        svn(&[
            "update",
            "-q",
            "--set-depth",
            "exclude",
            &format!("{t}/src"),
        ]);
        svn(&[
            "switch",
            "-q",
            &format!("{file_url}/branches/b@{}", branched.0),
            t,
        ]);
        assert_eq!(rows, base_rows(&theirs), "server {version}");
    }
    assert_no_prompt();
}

/// Switching the working copy while `src` is switched on its own to an unrelated location:
/// `src` comes back below the root at the new branch, read from where it was switched to
/// (`link_path`), as svn 1.15 leaves it on the same repository.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn (see the module docs)"]
async fn switching_the_parent_of_a_switched_folder_matches_svn() {
    use opensvn_lib::engine::real::SessionConnector as _;
    for (version, host) in servers() {
        eprintln!("server {version}");
        let h = harness(&host, "switch-parent");
        let url = h.repo_url();
        let e = h.engine();
        for to in ["branches/b", "branches/c"] {
            opensvn_lib::write_op!(
                &h,
                e.copy_location(
                    &CopyLocationRequest {
                        from_url: format!("{url}/trunk"),
                        from_rev: None,
                        to_url: format!("{url}/{to}"),
                        message: "Branch".into(),
                        switch_wc_id: None,
                        from_wc: None,
                    },
                    None,
                    &ctx(),
                )
            )
            .expect("copy");
        }
        let root = h.work_dir().join("parent");
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
            id: WcId("parent".into()),
            root: PathBuf::from(info.root_path),
        };
        // `src` to the whole branch `b` (another path, so ancestry is ignored, as svn needs).
        let cancel = svn_types::CancelToken::new();
        let session = connector()
            .connect_with(
                &svn_types::RepoUrl::parse(&format!("{url}/trunk")).unwrap(),
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
            true,
            &cancel,
        )
        .await
        .expect("switch src");
        assert!(wc.root.join("src/README.md").exists());
        e.switch(&wc, &format!("{url}/branches/c"), None, false, &ctx())
            .await
            .expect("switch the working copy");
        let rows = base_rows(&wc.root);
        assert!(
            rows.iter()
                .any(|r| r.starts_with("src/main.rs|normal|file|")
                    && r.contains("branches/c/src/main.rs")),
            "{rows:#?}"
        );
        assert!(!wc.root.join("src/README.md").exists());

        let (Host::Local { repos, .. }, Some(oracle)) = (
            &host,
            opensvn_svn_oracle::Oracle::resolve(opensvn_svn_oracle::MinVersion::V1_15)
                .ok()
                .flatten(),
        ) else {
            eprintln!("no local repository or svn 1.15: the svn comparison is skipped");
            continue;
        };
        let file_url = opensvn_svn_oracle::Oracle::file_url(&repos.join(&h.scratch.name));
        let theirs = h.tmp.path().join("svn-wc");
        let t = theirs.to_str().unwrap();
        let src = format!("{t}/src");
        for args in [
            vec!["checkout", "-q", &format!("{file_url}/trunk@1")[..], t],
            vec![
                "switch",
                "-q",
                "--ignore-ancestry",
                &format!("{file_url}/branches/b@3")[..],
                &src[..],
            ],
            vec!["switch", "-q", &format!("{file_url}/branches/c@3")[..], t],
        ] {
            let out = oracle.svn().args(&args).output().unwrap();
            assert!(
                out.status.success(),
                "svn {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        assert_eq!(rows, base_rows(&theirs), "server {version}");
    }
    assert_no_prompt();
}

// ---- update of a working copy with a switched subtree ----

/// An update of a working copy whose `src` is switched to a branch takes `src` from the branch
/// (the reporter sends it with `link_path`, and mod_dav_svn follows the link in the target as
/// svn_repos does): the branch's edit arrives, trunk's does not. The same sequence on the fake
/// session is `crates/svn-client/tests/update_switched_subtree_vs_svn.rs`; here the rows are
/// compared with svn 1.15's own update of the same repository (over `file://`).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn (see the module docs)"]
async fn update_of_a_switched_folder_takes_it_from_its_branch_like_svn() {
    use opensvn_lib::engine::real::SessionConnector as _;
    for (version, host) in servers() {
        eprintln!("server {version}");
        let h = harness(&host, "update-switched");
        let url = h.repo_url();
        let e = h.engine();
        // r2: the branch.
        opensvn_lib::write_op!(
            &h,
            e.copy_location(
                &CopyLocationRequest {
                    from_url: format!("{url}/trunk"),
                    from_rev: None,
                    to_url: format!("{url}/branches/b"),
                    message: "Branch".into(),
                    switch_wc_id: None,
                    from_wc: None,
                },
                None,
                &ctx(),
            )
        )
        .expect("branch");
        let wc = checkout_trunk(&h, "switched").await;
        let cancel = svn_types::CancelToken::new();
        let session = connector()
            .connect_with(
                &svn_types::RepoUrl::parse(&format!("{url}/trunk/src")).unwrap(),
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
            &svn_types::RepoUrl::parse(&format!("{url}/branches/b/src")).unwrap(),
            None,
            svn_types::Depth::Unknown,
            false,
            &cancel,
        )
        .await
        .expect("switch src");
        // r3: an edit of `src/main.rs` on the branch; r4: another one on trunk.
        let commit_in = |at: &'static str, text: &'static str, name: &'static str| {
            let h = &h;
            async move {
                let plan = CheckoutPlan {
                    url: format!("{}/{at}", h.repo_url()),
                    path: h.work_dir().join(name),
                    rev: None,
                    depth: Depth::Infinity,
                    sparse: None,
                    format: WcFormat::V31,
                    store_pristine: true,
                    ignore_externals: false,
                };
                let info = h.engine.checkout(&plan, &ctx()).await.expect("checkout");
                let other = WcRef {
                    id: WcId(name.to_owned()),
                    root: PathBuf::from(info.root_path),
                };
                h.write_file(&other.root, "src/main.rs", text).unwrap();
                let mark = h.write_mark();
                match h
                    .engine
                    .commit(
                        &other,
                        &CommitRequest {
                            wc_id: other.id.clone(),
                            paths: vec!["src/main.rs".into()],
                            message: "Edit".into(),
                            keep_locks: false,
                            auto_add_remove: false,
                        },
                        &ctx(),
                    )
                    .await
                {
                    Ok(rev) => Some(rev),
                    Err(e) if conformance::broken_write_ends_scenario(h, mark, &e) => None,
                    Err(e) => panic!("commit: {e:?}"),
                }
            }
        };
        if commit_in("branches/b", "fn main() { branch() }\n", "branch-edit")
            .await
            .is_none()
        {
            continue;
        }
        let Some(head) = commit_in("trunk", "fn main() { trunk() }\n", "trunk-edit").await else {
            continue;
        };
        let reached = e
            .update(
                &wc,
                &UpdateRequest {
                    wc_id: wc.id.clone(),
                    paths: None,
                    rev: None,
                    depth: None,
                },
                &ctx(),
            )
            .await
            .expect("update");
        assert_eq!(reached, head);
        assert_eq!(
            std::fs::read_to_string(wc.root.join("src/main.rs")).unwrap(),
            "fn main() { branch() }\n"
        );
        let rows = nodes_rows(&wc.root)
            .into_iter()
            .map(|r| r.rsplit_once('|').unwrap().0.to_owned())
            .collect::<Vec<_>>();
        assert!(
            rows.iter()
                .any(|r| r.starts_with("src/main.rs|0|normal|file|")
                    && r.contains("branches/b/src/main.rs")),
            "{rows:#?}"
        );

        let (Host::Local { repos, .. }, Some(oracle)) = (
            &host,
            opensvn_svn_oracle::Oracle::resolve(opensvn_svn_oracle::MinVersion::V1_15)
                .ok()
                .flatten(),
        ) else {
            eprintln!("no local repository or svn 1.15: the svn comparison is skipped");
            continue;
        };
        let file_url = opensvn_svn_oracle::Oracle::file_url(&repos.join(&h.scratch.name));
        let theirs = h.tmp.path().join("svn-wc");
        let t = theirs.to_str().unwrap();
        let src = format!("{t}/src");
        for args in [
            vec!["checkout", "-q", &format!("{file_url}/trunk@2")[..], t],
            vec![
                "switch",
                "-q",
                &format!("{file_url}/branches/b/src@2")[..],
                &src[..],
            ],
            vec!["update", "-q", t],
        ] {
            let out = oracle.svn().args(&args).output().unwrap();
            assert!(
                out.status.success(),
                "svn {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        // Authors differ (svn over file:// commits as the local user): compared without them.
        let theirs_rows = nodes_rows(&theirs)
            .into_iter()
            .map(|r| r.rsplit_once('|').unwrap().0.to_owned())
            .collect::<Vec<_>>();
        assert_eq!(rows, theirs_rows, "server {version}");
    }
    assert_no_prompt();
}

// ---- a switch with a narrow depth, cut and resumed ----

/// A switch with `--depth files` and `--depth immediates` through a proxy that cuts every
/// `REPORT` answer after 64 KiB: the drive breaks inside the files of the target, is resumed at
/// the switched target (the folders it has not finished reported with `link_path`), and ends
/// with the BASE rows svn 1.15's own switch at that depth leaves on the same repository (over
/// `file://`): nodes within the depth at the branch, the folders' content outside the depth
/// where it was.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn and scripts/test-env/visualsvn-like.sh certificates"]
async fn switch_with_a_narrow_depth_cut_mid_answer_matches_svn() {
    use opensvn_lib::engine::real::SessionConnector as _;
    const FILES: usize = 24;
    const SIZE: usize = 8192;
    if behind_a_faulty_proxy() {
        eprintln!("SKIPPED: starts its own flaky proxy, so it runs against the server directly");
        return;
    }
    let Some((_, host @ Host::Local { .. })) = servers().into_iter().next() else {
        eprintln!("SKIPPED: the flaky proxy runs in front of the local server only");
        return;
    };
    let Host::Local { base, repos } = &host else {
        unreachable!()
    };
    let Some(oracle) =
        opensvn_svn_oracle::Oracle::resolve(opensvn_svn_oracle::MinVersion::V1_15).unwrap()
    else {
        return;
    };
    for (depth, word) in [
        (svn_types::Depth::Files, "files"),
        (svn_types::Depth::Immediates, "immediates"),
    ] {
        let mut files = Vec::new();
        for f in 0..FILES {
            files.push((format!("branches/sw/top{f:02}.bin"), noise(SIZE)));
        }
        files.push(("branches/sw/deep/deep.bin".to_owned(), noise(SIZE)));
        let Some((h, wc, port)) =
            switch_setup(&host, base, &format!("switchdepth-{word}"), files).await
        else {
            eprintln!("SKIPPED: no server certificate next to the CA for the flaky proxy");
            return;
        };
        let cutting = proxy_on(
            base,
            port,
            &[
                "--truncate-every",
                "1",
                "--truncate-at",
                "65536",
                "--truncate-method",
                "REPORT",
            ],
            h.scratch_dir().join("proxy-1.log"),
        );
        let at = |rel: &str| {
            svn_types::RepoUrl::parse(&format!("{}/{}/{rel}", cutting.url(base), h.scratch.name))
                .unwrap()
        };
        let cancel = svn_types::CancelToken::new();
        let session = connector()
            .connect_with(
                &at("trunk"),
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
        let sum = svn_client::switch(
            &client,
            session.as_ref(),
            &wc.root,
            &at("branches/sw"),
            None,
            depth,
            false,
            &cancel,
        )
        .await
        .unwrap_or_else(|err| {
            panic!(
                "--depth {word}: {err:?}\n{}",
                std::fs::read_to_string(&cutting.log).unwrap_or_default()
            )
        });
        assert!(
            cutting.cuts() >= 2,
            "--depth {word}: {}",
            std::fs::read_to_string(&cutting.log).unwrap_or_default()
        );
        let reached = sum.revision.expect("revision").0;
        let rows = base_rows(&wc.root);

        let file_url = opensvn_svn_oracle::Oracle::file_url(&repos.join(&h.scratch.name));
        let theirs = h.tmp.path().join("svn-wc");
        let t = theirs.to_str().unwrap();
        for args in [
            vec![
                "checkout",
                "-q",
                &format!("{file_url}/trunk@{reached}")[..],
                t,
            ],
            vec![
                "switch",
                "-q",
                "--depth",
                word,
                &format!("{file_url}/branches/sw@{reached}")[..],
                t,
            ],
        ] {
            let out = oracle.svn().args(&args).output().unwrap();
            assert!(
                out.status.success(),
                "svn {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        assert_eq!(rows, base_rows(&theirs), "--depth {word}");
        assert_eq!(
            std::fs::read_to_string(wc.root.join("README.md")).unwrap(),
            "local edit\n"
        );
    }
    assert_no_prompt();
}

// ---- the RA layer's cache (`NODES.dav_cache`) ----

/// BASE rows with the RA layer's cache, the repository name masked (each client works on its
/// own copy of the same repository).
fn dav_rows(wc: &Path, repo: &str) -> Vec<String> {
    let db = WcDb::open(wc).unwrap();
    db.with_read(|c| {
        let mut s = c
            .prepare(
                "SELECT local_relpath, presence, revision, repos_path, checksum, dav_cache \
                 FROM nodes WHERE op_depth = 0 ORDER BY local_relpath",
            )
            .unwrap();
        let rows = s
            .query_map([], |r| {
                let dav: Option<Vec<u8>> = r.get(5)?;
                Ok(format!(
                    "{}|{}|{:?}|{:?}|{:?}|{}",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    dav.map(|d| {
                        let props = svn_wc::skel::parse_proplist(
                            &svn_wc::skel::Skel::parse(&d).expect("dav_cache skel"),
                        )
                        .expect("dav_cache properties");
                        props
                            .iter()
                            .map(|(k, v)| {
                                format!(
                                    "{}={}",
                                    String::from_utf8_lossy(k),
                                    String::from_utf8_lossy(v).replace(repo, "{repo}")
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(";")
                    })
                    .unwrap_or_else(|| "NULL".into()),
                ))
            })
            .unwrap();
        Ok(rows.map(|r| r.unwrap()).collect::<Vec<_>>())
    })
    .unwrap()
}

/// The engine fills `NODES.dav_cache` like svn 1.15 over mod_dav_svn: every file it receives
/// caches its `D:checked-in` href as `svn:wc:ra_dav:version-url` (names with spaces, `%`, `#`
/// and Vietnamese letters percent-encoded as the server sends them), a commit drops the cache
/// of what it committed, an update leaves untouched files alone, a switch empties the target's
/// cache before its drive, and cleanup empties it. The engine works on one copy of the
/// repository, svn on another, through the same steps; the rows must be equal at every step.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn (see the module docs)"]
async fn dav_cache_matches_svn() {
    let Some(oracle) =
        opensvn_svn_oracle::Oracle::resolve(opensvn_svn_oracle::MinVersion::V1_15).unwrap()
    else {
        return;
    };
    if negotiate_mode() {
        eprintln!("skipping: svn is given Basic credentials");
        return;
    }
    if proxy_cuts_answers() {
        eprintln!("SKIPPED: svn cannot resume an answer the proxy cuts (E120106)");
        return;
    }
    for (version, host) in servers() {
        eprintln!("server {version}");
        let h = harness(&host, "dav-cache");
        let theirs_repo = Scratch::new(&host, "dav-cache-svn");
        let e = h.engine();
        let ours_url = h.repo_url();
        let theirs_url = theirs_repo.url();
        let (ours_name, theirs_name) = (h.scratch.name.clone(), theirs_repo.name.clone());
        let tmp = h.tmp.path().to_path_buf();
        let svn = |cwd: &Path, args: &[&str]| svn_cli(&oracle, cwd, args);
        let ours = h.work_dir().join("ours");
        let theirs = tmp.join("theirs");
        let compare = |what: &str| {
            assert_eq!(
                dav_rows(&ours, &ours_name),
                dav_rows(&theirs, &theirs_name),
                "server {version}: {what}"
            );
        };
        let wc = WcRef {
            id: WcId("ours".into()),
            root: ours.clone(),
        };
        let update = || async {
            e.update(
                &wc,
                &UpdateRequest {
                    wc_id: wc.id.clone(),
                    paths: None,
                    rev: None,
                    depth: None,
                },
                &ctx(),
            )
            .await
            .expect("update")
        };
        let commit = |paths: Vec<String>| async {
            let mark = h.write_mark();
            match e
                .commit(
                    &wc,
                    &CommitRequest {
                        wc_id: wc.id.clone(),
                        paths,
                        message: "Edit".into(),
                        keep_locks: false,
                        auto_add_remove: true,
                    },
                    &ctx(),
                )
                .await
            {
                Ok(_) => true,
                Err(err) if conformance::broken_write_ends_scenario(&h, mark, &err) => false,
                Err(err) => panic!("commit: {err:?}"),
            }
        };
        let both = |rel: &str, text: &str| {
            for root in [&ours, &theirs] {
                let p = root.join(rel);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, text).unwrap();
            }
        };

        // Checkout of trunk, then files with names that need encoding, committed by each.
        e.checkout(
            &CheckoutPlan {
                url: format!("{ours_url}/trunk"),
                path: ours.clone(),
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
        svn(
            &tmp,
            &[
                "checkout",
                "-q",
                &format!("{theirs_url}/trunk"),
                theirs.to_str().unwrap(),
            ],
        );
        compare("checkout");
        let odd = ["Tài liệu/Đọc tôi %#.txt", "a b/x y.txt"];
        for f in odd {
            both(f, "odd\n");
        }
        svn(&theirs, &["add", "-q", "Tài liệu", "a b"]);
        if !commit(vec!["Tài liệu".into(), "a b".into()]).await {
            continue;
        }
        svn(&theirs, &["commit", "-q", "-m", "Edit"]);
        compare("commit of new files");
        update().await;
        svn(&theirs, &["update", "-q"]);
        compare("update after the commit");

        // An edit committed from a second working copy of each: the update brings it.
        let other = |name: &str| tmp.join(name);
        e.checkout(
            &CheckoutPlan {
                url: format!("{ours_url}/trunk"),
                path: other("ours-2"),
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
        .expect("second checkout");
        let second = WcRef {
            id: WcId("ours-2".into()),
            root: other("ours-2"),
        };
        svn(
            &tmp,
            &[
                "checkout",
                "-q",
                &format!("{theirs_url}/trunk"),
                other("theirs-2").to_str().unwrap(),
            ],
        );
        for root in [other("ours-2"), other("theirs-2")] {
            std::fs::write(root.join("Tài liệu/Đọc tôi %#.txt"), "odd\nedited\n").unwrap();
        }
        let mark = h.write_mark();
        match e
            .commit(
                &second,
                &CommitRequest {
                    wc_id: second.id.clone(),
                    paths: vec!["Tài liệu/Đọc tôi %#.txt".into()],
                    message: "Edit".into(),
                    keep_locks: false,
                    auto_add_remove: false,
                },
                &ctx(),
            )
            .await
        {
            Ok(_) => {}
            Err(err) if conformance::broken_write_ends_scenario(&h, mark, &err) => continue,
            Err(err) => panic!("commit: {err:?}"),
        }
        svn(&other("theirs-2"), &["commit", "-q", "-m", "Edit"]);
        update().await;
        svn(&theirs, &["update", "-q"]);
        compare("update with an incoming edit");

        // A local edit keeps the cache; a commit of it drops it.
        both("README.md", "hello\nlocal\n");
        compare("local edit");
        if !commit(vec!["README.md".into()]).await {
            continue;
        }
        svn(&theirs, &["commit", "-q", "-m", "Edit"]);
        compare("commit of an edit");

        // A switch (to the same URL: nothing to send) empties the cache of the target.
        update().await;
        svn(&theirs, &["update", "-q"]);
        e.switch(&wc, &format!("{ours_url}/trunk"), None, false, &ctx())
            .await
            .expect("switch");
        svn(
            &theirs,
            &["switch", "-q", &format!("{theirs_url}/trunk"), "."],
        );
        compare("switch");
        // And an update to an older revision and back fills it again.
        e.update(
            &wc,
            &UpdateRequest {
                wc_id: wc.id.clone(),
                paths: None,
                rev: Some(opensvn_ipc::Rev(1)),
                depth: None,
            },
            &ctx(),
        )
        .await
        .expect("update -r 1");
        svn(&theirs, &["update", "-q", "-r", "1"]);
        compare("update -r 1");
        update().await;
        svn(&theirs, &["update", "-q"]);
        compare("update to HEAD");

        // svn 1.14 and 1.15 work on the engine's working copy while its cache is filled: each
        // reads, updates and commits through a copy of it (the copies leave `ours` as it is).
        assert!(
            dav_rows(&ours, &ours_name)
                .iter()
                .any(|r| r.contains("ra_dav:version-url")),
            "server {version}: the engine's cache is empty before the svn round trip"
        );
        for (name, bin) in [
            ("svn14", Some(PathBuf::from("/usr/bin/svn"))),
            ("svn15", None),
        ] {
            if bin.as_ref().is_some_and(|b| !b.exists()) {
                continue;
            }
            let copy = tmp.join(format!("ours-{name}"));
            let mut cp = Command::new("cp");
            cp.arg("-a").arg(&ours).arg(&copy);
            run(cp, None);
            let cli = |args: &[&str]| {
                let mut c = bin.as_ref().map_or_else(|| oracle.svn(), Command::new);
                c.current_dir(&copy)
                    .env("LC_ALL", "C.UTF-8")
                    .args([
                        "--username",
                        "tester",
                        "--password",
                        "tester-pass",
                        "--no-auth-cache",
                        "--non-interactive",
                        "--config-option",
                    ])
                    .arg(format!(
                        "servers:global:ssl-authority-files={}",
                        ca_path().display()
                    ))
                    .args(args);
                run(c, None);
            };
            cli(&["status"]);
            cli(&["info", "-R"]);
            cli(&["update", "-q"]);
            std::fs::write(
                copy.join("Tài liệu/Đọc tôi %#.txt"),
                format!("odd\nedited\n{name}\n"),
            )
            .unwrap();
            cli(&["commit", "-q", "-m", name]);
            cli(&["update", "-q"]);
            cli(&["status"]);
        }

        // Cleanup empties it.
        e.cleanup(
            &wc,
            &opensvn_ipc::CleanupRequest {
                wc_id: wc.id.clone(),
                remove_unversioned: None,
                remove_ignored: None,
                vacuum_pristines: None,
                break_foreign_locks: None,
            },
            &ctx(),
        )
        .await
        .expect("cleanup");
        svn(&theirs, &["cleanup"]);
        compare("cleanup");
        // svn 1.14 and 1.15 take the engine's working copy as it is.
        for bin in [Some("/usr/bin/svn"), None] {
            let mut c = match bin {
                Some(b) if Path::new(b).exists() => Command::new(b),
                Some(_) => continue,
                None => oracle.svn(),
            };
            let out = c
                .current_dir(&ours)
                .env("LC_ALL", "C.UTF-8")
                .args(["status", "--xml"])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    assert_no_prompt();
}

// ---- log with merged revisions (`svn log -g`) ----

/// `svn` against the test server, returning its standard output.
fn svn_out(oracle: &opensvn_svn_oracle::Oracle, cwd: &Path, args: &[&str]) -> String {
    let mut c = oracle.svn();
    c.current_dir(cwd)
        .args([
            "--username",
            "tester",
            "--password",
            "tester-pass",
            "--no-auth-cache",
            "--config-option",
        ])
        .arg(format!(
            "servers:global:ssl-authority-files={}",
            ca_path().display()
        ))
        .args(args);
    let out = c.output().unwrap();
    assert!(
        out.status.success(),
        "{c:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// One entry of a log as both sides are compared: revision, author, message, reverse merge,
/// the extra revision properties and the merged-in entries below it.
#[derive(Debug, PartialEq, Eq)]
struct LogNode {
    rev: u64,
    author: Option<String>,
    msg: String,
    reverse: bool,
    revprops: Vec<(String, String)>,
    merged: Vec<LogNode>,
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The entries of `svn log --xml` (with `-g`, nested `<logentry>` elements are merged-in
/// revisions; `reverse-merge="true"` marks a reverse merge).
fn parse_svn_log(xml: &str) -> Vec<LogNode> {
    fn text_of(rest: &str, tag: &str) -> Option<(String, usize)> {
        let open = format!("<{tag}");
        let rest_t = rest.trim_start();
        if !rest_t.starts_with(&open) {
            return None;
        }
        let skip = rest.len() - rest_t.len();
        let start = rest_t.find('>')? + 1;
        let close = format!("</{tag}>");
        let end = rest_t.find(&close)?;
        Some((xml_unescape(&rest_t[start..end]), skip + end + close.len()))
    }
    let mut stack: Vec<LogNode> = Vec::new();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = xml[i..].find('<') {
        i += off;
        let rest = &xml[i..];
        if rest.starts_with("<logentry") {
            let end = rest.find('>').unwrap();
            let head = &rest[..end];
            let attr = |name: &str| {
                head.find(&format!("{name}=\"")).map(|p| {
                    let v = &head[p + name.len() + 2..];
                    v[..v.find('"').unwrap()].to_owned()
                })
            };
            stack.push(LogNode {
                rev: attr("revision").unwrap().parse().unwrap(),
                author: None,
                msg: String::new(),
                reverse: attr("reverse-merge").as_deref() == Some("true"),
                revprops: Vec::new(),
                merged: Vec::new(),
            });
            i += end + 1;
        } else if rest.starts_with("</logentry>") {
            let done = stack.pop().unwrap();
            match stack.last_mut() {
                Some(parent) => parent.merged.push(done),
                None => out.push(done),
            }
            i += "</logentry>".len();
        } else if let Some((t, n)) = text_of(rest, "author") {
            stack.last_mut().unwrap().author = Some(t);
            i += n;
        } else if let Some((t, n)) = text_of(rest, "msg") {
            stack.last_mut().unwrap().msg = t;
            i += n;
        } else if rest
            .strip_prefix("<property")
            .is_some_and(|r| r.starts_with(char::is_whitespace))
        {
            // svn writes the attribute on its own line: `<property\n   name="...">`.
            let name_at = rest.find("name=\"").unwrap() + 6;
            let name = &rest[name_at..name_at + rest[name_at..].find('"').unwrap()];
            let start = rest.find('>').unwrap() + 1;
            let end = rest.find("</property>").unwrap();
            stack
                .last_mut()
                .unwrap()
                .revprops
                .push((name.to_owned(), xml_unescape(&rest[start..end])));
            i += end + "</property>".len();
        } else {
            i += 1;
        }
    }
    out
}

fn engine_log_nodes(entries: Vec<opensvn_ipc::LogEntry>) -> Vec<LogNode> {
    entries
        .into_iter()
        .map(|e| LogNode {
            rev: e.rev.0,
            author: e.author,
            msg: e.message,
            reverse: e.subtractive_merge.unwrap_or(false),
            revprops: e.revprops.unwrap_or_default().into_iter().collect(),
            merged: engine_log_nodes(e.merged.unwrap_or_default()),
        })
        .collect()
}

/// `log_get` with merged revisions gives the tree of `svn log -g` (svn 1.15) for the same URL:
/// a merge of a branch that itself merged another branch (nesting two deep), a reverse merge,
/// the limit counting only the node's own entries, and an extra revision property.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn (see the module docs)"]
async fn log_with_merged_revisions_matches_svn_log_g() {
    let Some(oracle) =
        opensvn_svn_oracle::Oracle::resolve(opensvn_svn_oracle::MinVersion::V1_15).unwrap()
    else {
        return;
    };
    if negotiate_mode() {
        eprintln!("skipping: svn is given Basic credentials");
        return;
    }
    for (version, host) in servers() {
        eprintln!("log_with_merged_revisions_matches_svn_log_g: server {version}");
        let h = harness(&host, "log-g");
        // svn does not retry a cut response: behind a faulty proxy, the history is built and
        // logged by svn straight from the repository; only the engine goes through the proxy.
        let url = match &host {
            Host::Local { repos, .. } if behind_a_faulty_proxy() => {
                format!("file://{}", repos.join(&h.scratch.name).display())
            }
            _ => h.repo_url(),
        };
        let work = h.work_dir();
        std::fs::create_dir_all(&work).unwrap();
        let svn = |cwd: &Path, args: &[&str]| svn_cli(&oracle, cwd, args);
        let ours_trunk = format!("{}/trunk", h.repo_url());
        let trunk = format!("{url}/trunk");
        let b = format!("{url}/branches/b");
        let c = format!("{url}/branches/c");
        svn(&work, &["copy", "-q", "-m", "branch b", &trunk, &b]);
        svn(&work, &["copy", "-q", "-m", "branch c", &b, &c]);
        svn(&work, &["checkout", "-q", &c, "c"]);
        let cdir = work.join("c");
        std::fs::write(cdir.join("README.md"), "on c\n").unwrap();
        svn(&cdir, &["commit", "-q", "-m", "work on c"]);
        svn(&work, &["checkout", "-q", &b, "b"]);
        let bdir = work.join("b");
        std::fs::write(bdir.join("src/main.rs"), "fn main() { b(); }\n").unwrap();
        svn(&bdir, &["commit", "-q", "-m", "work on b"]);
        svn(&bdir, &["update", "-q"]);
        svn(&bdir, &["merge", "-q", &c]);
        svn(&bdir, &["commit", "-q", "-m", "merge c into b"]);
        svn(&work, &["checkout", "-q", &trunk, "t"]);
        let tdir = work.join("t");
        svn(&tdir, &["merge", "-q", &b]);
        svn(
            &tdir,
            &[
                "commit",
                "-q",
                "-m",
                "merge b into trunk",
                "--with-revprop",
                "opensvn:test=merged <b>",
            ],
        );
        svn(&tdir, &["update", "-q"]);
        // Undo the merge of "work on b" (a reverse merge of one revision of b).
        let work_on_b = svn_out(
            &oracle,
            &work,
            &[
                "log",
                "-q",
                "-l",
                "1",
                "--xml",
                "-r",
                "HEAD:1",
                &format!("{b}/src/main.rs"),
            ],
        );
        let rev_b = parse_svn_log(&work_on_b)[0].rev;
        svn(&tdir, &["merge", "-q", "-c", &format!("-{rev_b}"), &b]);
        svn(&tdir, &["commit", "-q", "-m", "revert work on b"]);

        for limit in [100u32, 1, 2] {
            let xml = svn_out(
                &oracle,
                &work,
                &[
                    "log",
                    "-g",
                    "--xml",
                    "-l",
                    &limit.to_string(),
                    "--with-revprop",
                    "svn:author",
                    "--with-revprop",
                    "svn:log",
                    "--with-revprop",
                    "svn:date",
                    "--with-revprop",
                    "opensvn:test",
                    &trunk,
                ],
            );
            let mut theirs = parse_svn_log(&xml);
            // svn lists the requested standard properties as properties too; ours reports them
            // as fields.
            fn strip(nodes: &mut [LogNode]) {
                for n in nodes {
                    n.revprops.retain(|(k, _)| !k.starts_with("svn:"));
                    strip(&mut n.merged);
                }
            }
            strip(&mut theirs);
            let page = h
                .engine()
                .log(
                    &opensvn_lib::engine::LogScope::Url(ours_trunk.clone()),
                    &opensvn_ipc::LogGetRequest {
                        target: opensvn_ipc::LogTarget::Url {
                            url: ours_trunk.clone(),
                        },
                        from_rev: None,
                        to_rev: None,
                        limit,
                        stop_on_copy: None,
                        with_paths: false,
                        filter: None,
                        include_merged: Some(true),
                        revprops: Some(vec!["opensvn:test".into()]),
                    },
                    &ctx(),
                )
                .await
                .expect("log -g");
            let ours = engine_log_nodes(page.entries);
            assert_eq!(ours, theirs, "limit {limit}");
            if limit == 100 {
                let merge = ours
                    .iter()
                    .find(|n| n.msg == "merge b into trunk")
                    .expect("the merge of b");
                assert_eq!(
                    merge.revprops,
                    vec![("opensvn:test".to_owned(), "merged <b>".to_owned())]
                );
                let nested = merge
                    .merged
                    .iter()
                    .find(|n| n.msg == "merge c into b")
                    .expect("b's merge of c, merged into trunk");
                assert!(nested.merged.iter().any(|n| n.msg == "work on c"));
                let revert = ours
                    .iter()
                    .find(|n| n.msg == "revert work on b")
                    .expect("the reverse merge");
                assert!(revert.merged.iter().all(|n| n.reverse));
                assert!(!revert.merged.is_empty());
            }
        }
    }
    assert_no_prompt();
}

/// `log_get` of a branch follows its copy like `svn log` (svn 1.15) when more entries than the
/// first request of a page asks for (16) lie before the copy: the next request cannot start
/// below the copy on the branch's path, which did not exist there. With stop-on-copy the page
/// ends at the copy, like `svn log --stop-on-copy`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn (see the module docs)"]
async fn log_of_a_branch_follows_its_copy_past_the_first_request() {
    let Some(oracle) =
        opensvn_svn_oracle::Oracle::resolve(opensvn_svn_oracle::MinVersion::V1_15).unwrap()
    else {
        return;
    };
    if negotiate_mode() {
        eprintln!("skipping: svn is given Basic credentials");
        return;
    }
    for (version, host) in servers() {
        eprintln!("log_of_a_branch_follows_its_copy_past_the_first_request: server {version}");
        let h = harness(&host, "log-copy");
        // As in `log_with_merged_revisions_matches_svn_log_g`: behind a faulty proxy svn works
        // on the repository directly.
        let url = match &host {
            Host::Local { repos, .. } if behind_a_faulty_proxy() => {
                format!("file://{}", repos.join(&h.scratch.name).display())
            }
            _ => h.repo_url(),
        };
        let work = h.work_dir();
        std::fs::create_dir_all(&work).unwrap();
        let svn = |cwd: &Path, args: &[&str]| svn_cli(&oracle, cwd, args);
        let trunk = format!("{url}/trunk");
        let long = format!("{url}/branches/long");
        let ours_long = format!("{}/branches/long", h.repo_url());
        svn(&work, &["checkout", "-q", &trunk, "t"]);
        let tdir = work.join("t");
        for i in 0..20 {
            std::fs::write(tdir.join("README.md"), format!("trunk {i}\n")).unwrap();
            svn(&tdir, &["commit", "-q", "-m", &format!("trunk {i}")]);
        }
        svn(&work, &["copy", "-q", "-m", "branch long", &trunk, &long]);
        svn(&work, &["checkout", "-q", &long, "l"]);
        let ldir = work.join("l");
        for i in 0..3 {
            std::fs::write(ldir.join("README.md"), format!("branch {i}\n")).unwrap();
            svn(&ldir, &["commit", "-q", "-m", &format!("branch {i}")]);
        }
        for stop_on_copy in [false, true] {
            let mut args = vec!["log", "--xml", "-l", "50"];
            if stop_on_copy {
                args.push("--stop-on-copy");
            }
            args.push(&long);
            let theirs = parse_svn_log(&svn_out(&oracle, &work, &args));
            assert!(stop_on_copy || theirs.len() > 20, "{theirs:?}");
            let page = h
                .engine()
                .log(
                    &opensvn_lib::engine::LogScope::Url(ours_long.clone()),
                    &opensvn_ipc::LogGetRequest {
                        target: opensvn_ipc::LogTarget::Url {
                            url: ours_long.clone(),
                        },
                        from_rev: None,
                        to_rev: None,
                        limit: 50,
                        stop_on_copy: Some(stop_on_copy),
                        with_paths: false,
                        filter: None,
                        include_merged: None,
                        revprops: None,
                    },
                    &ctx(),
                )
                .await
                .unwrap_or_else(|e| panic!("log, stop on copy {stop_on_copy}: {e:?}"));
            assert_eq!(page.next_rev, None, "stop on copy {stop_on_copy}");
            assert_eq!(
                engine_log_nodes(page.entries),
                theirs,
                "stop on copy {stop_on_copy}"
            );
        }
    }
    assert_no_prompt();
}
