//! A checkout that loses its connection, through the whole app (commands, operation manager,
//! registry, checkout journal, real engine) against Apache + mod_dav_svn, with
//! `scripts/test-env/flaky-tls-proxy.py` cutting the network:
//!
//! - an outage of 90 s mid-checkout is waited out and the checkout resumes at its revision;
//! - an outage longer than the wait leaves the working copy incomplete; once the network is
//!   back, "Continue checkout" finishes it (and the checkout dialog offers it for the folder);
//! - the app is killed mid-checkout; after a restart the working copy is listed as an
//!   interrupted checkout, its stale lock is ours and is cleaned without asking, and Continue
//!   finishes it.
//!
//! Each result is compared with `svn checkout -r R` of svn 1.15 (`OPENSVN_SVN_BIN`): working
//! files, NODES rows and `svn status`.
//!
//! Needs the environment of `scripts/test-env/visualsvn-like.sh env` and a Basic location:
//!
//! ```text
//! eval "$(scripts/test-env/visualsvn-like.sh env)"
//! OPENSVN_COMPAT_LOCAL_URL=https://svn.opensvn.test:18443/svn-basic \
//!   cargo test -p opensvn --features real-engine,engine-conformance --lib dav_resume -- --ignored
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use opensvn_ipc::{
    AppError, CheckoutRequest, Depth, GrantKind, OpResult, OpStage, OperationHandle, SettingsPatch,
    WcFormat, WorkingCopy,
};
use opensvn_svn_oracle::{MinVersion, Oracle};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject as _;
use svn_ra::transport::ProxyConfig;
use svn_ra_api::{
    AuthMethod, AuthPrompter, CertDecision, CertInfo, CredentialRequest, Credentials,
};
use tauri::Manager;
use tauri::test::MockRuntime;

use super::fixture::{
    RecordingLauncher, RecordingMenu, RecordingNotifier, RecordingOpener, RecordingUpdates,
    Scripted, ScriptedPaths,
};
use crate::app_state::{AppDeps, AppState, ENGINE_STATE_SUBDIR, EngineSource};
use crate::engine::real::{ClientRecovery, HttpConnector, RealEngine, RealEngineConfig};
use crate::ops::events::{Recorded, RecordingEvents};
use crate::path_grants::MAIN_WINDOW;
use crate::status::StatusTiming;
use crate::updater::MemorySecretStore;

/// The local server, or `None` (the test reports a skip).
struct Server {
    base: String,
    repos: PathBuf,
    svnadmin: String,
    ca: PathBuf,
}

fn server() -> Option<Server> {
    let base = std::env::var("OPENSVN_COMPAT_LOCAL_URL").ok()?;
    Some(Server {
        base: base.trim_end_matches('/').to_owned(),
        repos: std::env::var_os("OPENSVN_COMPAT_LOCAL_REPOS")?.into(),
        svnadmin: std::env::var("OPENSVN_COMPAT_LOCAL_SVNADMIN")
            .unwrap_or_else(|_| "svnadmin".into()),
        ca: std::env::var_os("OPENSVN_COMPAT_LOCAL_CA")?.into(),
    })
}

fn run(mut cmd: Command, stdin: Option<&[u8]>) -> String {
    use std::io::Write as _;
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `files` files of about `size` bytes in folders of 50, as one revision.
fn big_dump(files: usize, size: usize) -> Vec<u8> {
    let mut out = String::from("SVN-fs-dump-format-version: 2\n\n");
    let rp = "K 10\nsvn:author\nV 5\nalice\nK 7\nsvn:log\nV 3\nbig\nPROPS-END\n";
    out.push_str(&format!(
        "Revision-number: 1\nProp-content-length: {0}\nContent-length: {0}\n\n{rp}\n",
        rp.len()
    ));
    let empty = "PROPS-END\n";
    for d in 0..files.div_ceil(50) {
        out.push_str(&format!(
            "Node-path: d{d:03}\nNode-kind: dir\nNode-action: add\nProp-content-length: {0}\n\
             Content-length: {0}\n\n{empty}\n\n",
            empty.len()
        ));
    }
    for i in 0..files {
        let mut text = String::new();
        let mut x: u64 = (i as u64).wrapping_mul(2_654_435_761) | 1;
        while text.len() < size {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            text.push_str(&format!("{x:016x}\n"));
        }
        out.push_str(&format!(
            "Node-path: d{:03}/f{i:05}.txt\nNode-kind: file\nNode-action: add\n\
             Prop-content-length: {}\nText-content-length: {}\nContent-length: {}\n\n\
             {empty}{text}\n\n",
            i / 50,
            empty.len(),
            text.len(),
            empty.len() + text.len()
        ));
    }
    out.into_bytes()
}

/// A scratch repository of the local server; deleted when dropped.
struct Repo {
    dir: PathBuf,
    name: String,
}

impl Repo {
    fn new(s: &Server, tag: &str, files: usize, size: usize) -> Self {
        let name = format!("resume-{tag}-{}", std::process::id());
        let dir = s.repos.join(&name);
        let _ = std::fs::remove_dir_all(&dir);
        let mut c = Command::new(&s.svnadmin);
        c.arg("create").arg(&dir);
        run(c, None);
        let mut c = Command::new(&s.svnadmin);
        c.args(["load", "-q"]).arg(&dir);
        run(c, Some(&big_dump(files, size)));
        Self { dir, name }
    }

    fn file_url(&self) -> String {
        Oracle::file_url(&self.dir)
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The flaky proxy in front of the server; killed when dropped.
struct Proxy {
    child: std::process::Child,
    log: PathBuf,
    port: u16,
}

impl Proxy {
    fn start(s: &Server, args: &[&str], log: PathBuf) -> Self {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let certs = s.ca.parent().unwrap();
        let url = url::Url::parse(&s.base).unwrap();
        let upstream = format!(
            "{}:{}",
            url.host_str().unwrap(),
            url.port_or_known_default().unwrap()
        );
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../scripts/test-env/flaky-tls-proxy.py");
        let child = Command::new("python3")
            .arg(script)
            .args(["--listen", &port.to_string(), "--upstream", &upstream])
            .arg("--cert")
            .arg(certs.join("server.crt"))
            .arg("--key")
            .arg(certs.join("server.key"))
            .arg("--ca")
            .arg(&s.ca)
            .args(args)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .expect("python3");
        let started = Instant::now();
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(started.elapsed() < Duration::from_secs(10), "proxy");
            std::thread::sleep(Duration::from_millis(50));
        }
        Self { child, log, port }
    }

    fn url(&self, s: &Server, repo: &Repo) -> String {
        let mut url = url::Url::parse(&s.base).unwrap();
        url.set_port(Some(self.port)).unwrap();
        format!("{}/{}", url.as_str().trim_end_matches('/'), repo.name)
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    async fn wait_for(&self, line: &str, limit: Duration) {
        let started = Instant::now();
        while !self.log().contains(line) {
            assert!(started.elapsed() < limit, "proxy never logged {line:?}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The Basic account of `/svn-basic`; any unknown certificate is refused (the test CA is a root).
struct Tester;

#[async_trait]
impl AuthPrompter for Tester {
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

/// The app over the real engine, as `lib.rs` builds it, with the test CA as an extra root.
struct App {
    app: tauri::App<MockRuntime>,
    events: Arc<RecordingEvents>,
}

impl App {
    fn new(s: &Server, data_dir: &Path, wait_min: u32, timeout_sec: u32) -> Self {
        let der = CertificateDer::from_pem_file(&s.ca).unwrap();
        let engine = EngineSource::Build(Box::new(move |services| {
            let connector = HttpConnector::with_config_tweak(
                Arc::new(Tester),
                services.auth.trust_store(),
                services.settings.subscribe(),
                services.auth.invalidations(),
                move |c| {
                    c.proxy = ProxyConfig::Disabled;
                    c.extra_root_certs = vec![der.clone()];
                },
            );
            let state_dir =
                crate::preview::prepare_private_dir(&services.data_dir, ENGINE_STATE_SUBDIR)?;
            let engine: Arc<dyn crate::engine::EngineFacade> =
                Arc::new(RealEngine::new(RealEngineConfig {
                    connector: Arc::new(connector),
                    lock_ownership: Arc::new(
                        crate::lock_ownership::SqliteLockOwnership::with_os_probe(
                            services.db.clone(),
                        ),
                    ),
                    boot_id: crate::lock_ownership::current_boot_id().to_owned(),
                    state_dir,
                    recovery: Arc::new(ClientRecovery),
                }));
            Ok(engine)
        }));
        let events = Arc::new(RecordingEvents::default());
        let state = AppState::build(AppDeps {
            data_dir: data_dir.to_path_buf(),
            engine,
            credentials: Arc::new(secure_store::MemoryStore::new()),
            events: events.clone(),
            picker: Arc::new(Scripted::default()),
            status_timing: StatusTiming::default(),
            path_picker: Arc::new(ScriptedPaths::default()),
            opener: Arc::new(RecordingOpener::default()),
            notifier: Arc::new(RecordingNotifier::default()),
            menu: Arc::new(RecordingMenu::default()),
            updates: Arc::new(RecordingUpdates::default()),
            secrets: Arc::new(MemorySecretStore::default()),
            launcher: Arc::new(RecordingLauncher::default()),
        })
        .unwrap();
        state
            .settings
            .apply_patch(&SettingsPatch {
                network_reconnect_wait_min: Some(wait_min),
                network_timeout_sec: Some(timeout_sec),
                ..SettingsPatch::default()
            })
            .unwrap();
        let app = tauri::test::mock_app();
        app.manage(state);
        Self { app, events }
    }

    fn state(&self) -> tauri::State<'_, AppState> {
        self.app.state::<AppState>()
    }

    async fn checkout(
        &self,
        url: &str,
        parent: &Path,
        name: &str,
    ) -> Result<OperationHandle, AppError> {
        let dest = self
            .state()
            .grants
            .mint(parent.to_path_buf(), GrantKind::Dir, MAIN_WINDOW)
            .id;
        crate::commands::checkout(
            self.state(),
            CheckoutRequest {
                url: url.to_owned(),
                dest,
                dest_name: Some(name.to_owned()),
                rev: None,
                depth: Depth::Infinity,
                sparse: None,
                format: WcFormat::V31,
                store_pristine: true,
                ignore_externals: false,
            },
        )
        .await
    }

    async fn wait_op(&self, h: &OperationHandle, limit: Duration) -> Result<OpResult, AppError> {
        let started = Instant::now();
        loop {
            for r in self.events.all() {
                match r {
                    Recorded::Finished(f) if f.op_id == h.op_id => return Ok(f.result),
                    Recorded::Failed(f) if f.op_id == h.op_id => return Err(f.error),
                    _ => {}
                }
            }
            assert!(started.elapsed() < limit, "{} did not end", h.title);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Progress samples of `h` that said it waits for the connection.
    fn waits(&self, h: &OperationHandle) -> Vec<opensvn_ipc::ConnectionWait> {
        self.events
            .all()
            .into_iter()
            .filter_map(|r| match r {
                Recorded::Progress(p)
                    if p.op_id == h.op_id && p.stage == Some(OpStage::WaitingForConnection) =>
                {
                    p.connection_wait
                }
                _ => None,
            })
            .collect()
    }

    fn cleanup_banners(&self) -> usize {
        self.events
            .all()
            .into_iter()
            .filter(|r| {
                matches!(r, Recorded::Banner(b) if b.kind == opensvn_ipc::BannerKind::NeedsCleanup)
            })
            .count()
    }

    async fn listed(&self, root: &Path) -> WorkingCopy {
        let root = dunce::canonicalize(root).unwrap();
        let list = crate::commands::wc_list(self.state()).await.unwrap();
        list.into_iter()
            .find(|w| dunce::canonicalize(&w.root_path).unwrap() == root)
            .unwrap_or_else(|| panic!("{} is not listed", root.display()))
    }
}

fn oracle() -> Oracle {
    Oracle::resolve(MinVersion::V1_15)
        .expect("oracle")
        .expect("svn 1.15 (OPENSVN_SVN_BIN)")
}

/// Commits r2 straight to the server (not through the proxy), so HEAD moves past the revision
/// an interrupted checkout was reaching.
fn commit_r2(s: &Server, repo: &Repo) {
    let mut c = oracle().svn();
    c.args([
        "mkdir",
        "-m",
        "r2",
        "--username",
        "tester",
        "--password",
        "tester-pass",
        "--no-auth-cache",
        "--config-option",
    ])
    .arg(format!(
        "servers:global:ssl-authority-files={}",
        s.ca.display()
    ))
    .arg(format!("{}/{}/later", s.base, repo.name));
    run(c, None);
}

/// Files below `root` (without `.svn`) with their contents.
fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
            if rel == ".svn" {
                continue;
            }
            if p.is_dir() {
                out.insert(format!("{rel}/"), Vec::new());
                walk(base, &p, out);
            } else {
                out.insert(rel, std::fs::read(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// NODES rows without what depends on the transport or the clock.
fn nodes(root: &Path) -> Vec<String> {
    let c = rusqlite::Connection::open(root.join(".svn/wc.db")).unwrap();
    let mut stmt = c
        .prepare(
            "SELECT local_relpath, op_depth, IFNULL(parent_relpath, ''), IFNULL(repos_path, ''), \
             IFNULL(revision, -1), presence, kind, IFNULL(depth, ''), IFNULL(checksum, ''), \
             IFNULL(hex(properties), ''), IFNULL(changed_revision, -1), \
             IFNULL(changed_author, '') FROM nodes ORDER BY local_relpath, op_depth",
        )
        .unwrap();
    stmt.query_map([], |r| {
        let mut cols = Vec::new();
        for i in 0..12 {
            let v: rusqlite::types::Value = r.get(i)?;
            cols.push(format!("{v:?}"));
        }
        Ok(cols.join("|"))
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

fn svn_status(root: &Path) -> String {
    let mut c = oracle().svn();
    c.arg("status").current_dir(root);
    run(c, None)
}

/// The working copy equals `svn checkout -r rev` of the repository.
fn assert_matches_svn_checkout(repo: &Repo, ours: &Path, rev: u64, scratch: &Path) {
    let theirs = scratch.join(format!("svn-r{rev}"));
    let _ = std::fs::remove_dir_all(&theirs);
    let mut c = oracle().svn();
    c.args(["checkout", "-q", "-r", &rev.to_string(), &repo.file_url()])
        .arg(&theirs);
    run(c, None);
    assert_eq!(tree(ours), tree(&theirs), "working files differ");
    assert_eq!(nodes(ours), nodes(&theirs), "NODES rows differ");
    assert_eq!(svn_status(ours), "", "svn status of ours");
    assert_eq!(svn_status(&theirs), "", "svn status of svn's");
}

fn new_rev(r: &OpResult) -> u64 {
    match r {
        OpResult::CheckedOut { new_rev, .. } | OpResult::NewRev { new_rev } => new_rev.0,
        other => panic!("unexpected result {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn and scripts/test-env/visualsvn-like.sh"]
async fn an_outage_of_90s_mid_checkout_is_waited_out_at_the_same_revision() {
    let Some(s) = server() else {
        eprintln!("SKIPPED: OPENSVN_COMPAT_LOCAL_URL is not set");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repo::new(&s, "outage90", 600, 16_000);
    let proxy = Proxy::start(
        &s,
        &["--outage-at-bytes", "2000000", "--outage-for", "90"],
        tmp.path().join("proxy.log"),
    );
    let app = App::new(&s, &tmp.path().join("data"), 10, 10);
    let parent = tmp.path().join("work");
    std::fs::create_dir_all(&parent).unwrap();
    let h = app
        .checkout(&proxy.url(&s, &repo), &parent, "wc")
        .await
        .unwrap();
    proxy
        .wait_for("network down", Duration::from_secs(120))
        .await;
    commit_r2(&s, &repo);
    let r = app.wait_op(&h, Duration::from_secs(600)).await.unwrap();
    assert_eq!(new_rev(&r), 1, "resumed at the revision it was reaching");
    let waits = app.waits(&h);
    assert!(
        !waits.is_empty(),
        "the UI was told it waits for the connection"
    );
    assert!(waits.iter().all(|w| w.max_wait_secs == 600), "{waits:?}");
    assert!(proxy.log().contains("network up again"));
    let wc = parent.join("wc");
    assert!(!app.listed(&wc).await.incomplete);
    assert!(!wc.join("later").exists());
    assert_matches_svn_checkout(&repo, &wc, 1, tmp.path());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn and scripts/test-env/visualsvn-like.sh"]
async fn an_outage_longer_than_the_wait_leaves_a_checkout_that_continue_finishes() {
    let Some(s) = server() else {
        eprintln!("SKIPPED: OPENSVN_COMPAT_LOCAL_URL is not set");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repo::new(&s, "outage-long", 600, 16_000);
    let proxy = Proxy::start(
        &s,
        &["--outage-at-bytes", "2000000", "--outage-for", "150"],
        tmp.path().join("proxy.log"),
    );
    // Waits one minute: the outage outlasts it.
    let app = App::new(&s, &tmp.path().join("data"), 1, 10);
    let parent = tmp.path().join("work");
    std::fs::create_dir_all(&parent).unwrap();
    let url = proxy.url(&s, &repo);
    let h = app.checkout(&url, &parent, "wc").await.unwrap();
    proxy
        .wait_for("network down", Duration::from_secs(120))
        .await;
    commit_r2(&s, &repo);
    let err = app
        .wait_op(&h, Duration::from_secs(400))
        .await
        .expect_err("gave up waiting");
    let info = err
        .incomplete_checkout
        .as_deref()
        .cloned()
        .unwrap_or_else(|| panic!("no Continue offered: {err:?}"));
    assert_eq!(info.rev.0, 1);
    assert!(!app.waits(&h).is_empty());
    let wc = parent.join("wc");
    let listed = app.listed(&wc).await;
    assert!(listed.incomplete);
    let resume = listed.resume.expect("resume info");
    assert!(resume.checkout);
    assert_eq!(resume.rev.0, 1);
    // Nothing was deleted, no lock is left, and no cleanup is asked for.
    assert!(wc.join(".svn/wc.db").exists());
    assert_eq!(app.cleanup_banners(), 0);
    // The checkout dialog, same folder and URL: Continue is offered instead of "not empty".
    let again = app
        .checkout(&url, &parent, "wc")
        .await
        .expect_err("the folder is not empty");
    assert_eq!(
        again
            .incomplete_checkout
            .as_deref()
            .map(|i| i.wc_id.clone()),
        Some(listed.id.clone())
    );
    // Back online: Continue finishes it at r1 although HEAD is r2.
    proxy
        .wait_for("network up again", Duration::from_secs(300))
        .await;
    let h = crate::commands::wc_continue(app.state(), listed.id.clone())
        .await
        .unwrap();
    let r = app.wait_op(&h, Duration::from_secs(300)).await.unwrap();
    assert_eq!(new_rev(&r), 1);
    let listed = app.listed(&wc).await;
    assert!(!listed.incomplete && listed.resume.is_none());
    assert!(app.state().db.checkout_journal_list().unwrap().is_empty());
    assert_matches_svn_checkout(&repo, &wc, 1, tmp.path());
}

const CHILD_ENV: &str = "OPENSVN_RESUME_CHILD";

/// The app process the next test kills: starts a checkout and waits. Does nothing unless
/// started by that test.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "started by killed_mid_checkout_then_continued_after_a_restart"]
async fn child_checkout_until_killed() {
    let Ok(spec) = std::env::var(CHILD_ENV) else {
        return;
    };
    let mut parts = spec.splitn(3, '\n');
    let (data, parent, url) = (
        PathBuf::from(parts.next().unwrap()),
        PathBuf::from(parts.next().unwrap()),
        parts.next().unwrap().to_owned(),
    );
    let s = server().unwrap();
    let app = App::new(&s, &data, 10, 10);
    let h = app.checkout(&url, &parent, "wc").await.unwrap();
    let _ = app.wait_op(&h, Duration::from_secs(3600)).await;
    panic!("the checkout ended before the test killed it");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Apache + mod_dav_svn and scripts/test-env/visualsvn-like.sh"]
async fn killed_mid_checkout_then_continued_after_a_restart() {
    let Some(s) = server() else {
        eprintln!("SKIPPED: OPENSVN_COMPAT_LOCAL_URL is not set");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repo::new(&s, "killed", 3000, 4_000);
    let data = tmp.path().join("data");
    let parent = tmp.path().join("work");
    std::fs::create_dir_all(&parent).unwrap();
    let url = format!("{}/{}", s.base, repo.name);
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "app_tests::dav_resume::child_checkout_until_killed",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env(
            CHILD_ENV,
            format!("{}\n{}\n{url}", data.display(), parent.display()),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Killed once some files are in: mid-drive, holding the working copy lock.
    let wc = parent.join("wc");
    let started = Instant::now();
    loop {
        let files = std::fs::read_dir(wc.join("d001")).map_or(0, Iterator::count);
        if files >= 10 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(300),
            "the child never started its checkout"
        );
        assert!(child.try_wait().unwrap().is_none(), "the child ended early");
        std::thread::sleep(Duration::from_millis(20));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    {
        let c = rusqlite::Connection::open(wc.join(".svn/wc.db")).unwrap();
        let locks: i64 = c
            .query_row("SELECT COUNT(*) FROM wc_lock", [], |r| r.get(0))
            .unwrap();
        assert!(locks > 0, "the killed checkout held the working copy lock");
    }

    // Restart: what `finish_startup` runs.
    let app = App::new(&s, &data, 10, 10);
    app.state().recover_checkouts().await;
    app.state().recover_all().await;
    let listed = app.listed(&wc).await;
    assert!(listed.incomplete, "listed as incomplete, not as an error");
    let resume = listed.resume.expect("resume info");
    assert!(resume.checkout, "an interrupted checkout");
    // The dead process was ours: its lock is gone without asking for a cleanup.
    assert_eq!(app.cleanup_banners(), 0);
    commit_r2(&s, &repo);
    let h = crate::commands::wc_continue(app.state(), listed.id.clone())
        .await
        .unwrap();
    let r = app.wait_op(&h, Duration::from_secs(600)).await.unwrap();
    assert_eq!(new_rev(&r), resume.rev.0);
    assert_eq!(resume.rev.0, 1);
    assert!(!app.listed(&wc).await.incomplete);
    assert_matches_svn_checkout(&repo, &wc, 1, tmp.path());
}
