//! Write-path compatibility against the pinned Subversion servers (1.14.5 on :8443, 1.15.0 on
//! :8444) and the `svn` command line as oracle.
//!
//! Needs Docker and the `svn` CLI: `cargo xtask compat up --no-build`, then
//! `cargo test -p svn-ra --test compat_write -- --ignored`. Each run creates a scratch
//! repository on the server and removes it again, so the fixture repositories stay untouched.
//!
//! Without Docker, point the tests at a local Apache + mod_dav_svn instead (HTTPS, Basic auth
//! `tester`/`tester-pass`, `SVNParentPath` writable by the test):
//!
//! - `OPENSVN_COMPAT_LOCAL_URL`: the parent-path URL, e.g. `https://localhost:18276/svn`;
//! - `OPENSVN_COMPAT_LOCAL_REPOS`: the `SVNParentPath` directory (scratch repositories are
//!   created there with the local `svnadmin`);
//! - `OPENSVN_COMPAT_CA`: the CA certificate of the server (PEM).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use md5::{Digest, Md5};
use svn_protocol::BulkPreference;
use svn_ra::ops::commit::{abort_txn, start_commit};
use svn_ra::ops::update::{RaWire, run_update};
use svn_ra::transport::{HttpTransport, MemoryTrustStore, ProxyConfig, TransportConfig};
use svn_ra_api::fake::RecordingEditor;
use svn_ra_api::{
    AuthMethod, AuthPrompter, CertDecision, CertInfo, CommitInfo, CommitObserver,
    CredentialRequest, Credentials, ReplayableSource, UpdateMode, UpdateRequest,
};
use svn_types::{
    CancelToken, Checksum, Depth, ErrorCode, PropChange, RelPath, RepoUrl, Result, Revnum,
};
use tempfile::NamedTempFile;

const SERVERS: [(&str, u16, &str); 2] = [
    ("1.14.5", 8443, "opensvn-compat-svn14-server-1"),
    ("1.15.0", 8444, "opensvn-compat-svn15-server-1"),
];

/// Where the scratch repositories of a server live.
#[derive(Clone)]
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

fn p(s: &str) -> RelPath {
    RelPath::new(s).expect("path")
}

fn ca_path() -> PathBuf {
    match std::env::var_os("OPENSVN_COMPAT_CA") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/compat/certs/ca.crt"),
    }
}

struct TesterPrompter;

#[async_trait::async_trait]
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

fn transport() -> Arc<HttpTransport> {
    let pem = std::fs::read_to_string(ca_path()).expect("test CA (cargo xtask compat up)");
    let b64: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .expect("CA base64");
    let config = TransportConfig {
        proxy: ProxyConfig::Disabled,
        extra_root_certs: vec![der.into()],
        ..TransportConfig::default()
    };
    Arc::new(HttpTransport::new(
        config,
        Arc::new(TesterPrompter),
        Arc::new(MemoryTrustStore::new()),
    ))
}

/// A scratch repository that is deleted when dropped.
struct Scratch {
    host: Host,
    name: String,
}

impl Scratch {
    fn new(host: &Host) -> Self {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let name = format!(
            "ra10b-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        );
        let out = match host {
            // The server runs Apache as uid 33; the repository must belong to that user.
            Host::Docker { container, .. } => Command::new("docker")
                .args(["exec", "-u", "33:33", container, "svnadmin", "create"])
                .arg(format!("/var/svn/repos/{name}"))
                .output()
                .expect("docker"),
            Host::Local { repos, .. } => Command::new("svnadmin")
                .arg("create")
                .arg(repos.join(&name))
                .output()
                .expect("svnadmin"),
        };
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
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

    /// URL for the CLI: Docker publishes the port on IPv4 only, and `svn` tries `::1` first.
    fn cli_url(&self) -> String {
        match &self.host {
            Host::Docker { port, .. } => format!("https://127.0.0.1:{port}/svn/{}", self.name),
            Host::Local { .. } => self.url(),
        }
    }

    fn cli(&self) -> Command {
        let mut c = Command::new("svn");
        c.args([
            "--non-interactive",
            "--no-auth-cache",
            "--username",
            "tester",
            "--password",
            "tester-pass",
        ])
        .arg("--trust-server-cert-failures=cn-mismatch")
        .arg(format!(
            "--config-option=servers:global:ssl-authority-files={}",
            ca_path().display()
        ));
        c
    }

    /// Runs the svn CLI against the scratch server and returns stdout.
    fn svn(&self, args: &[&str]) -> String {
        let out = self.cli().args(args).output().expect("svn CLI");
        assert!(
            out.status.success(),
            "svn {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn cat(&self, path: &str) -> Vec<u8> {
        let url = format!("{}/{path}", self.cli_url());
        let out = self.cli().args(["cat", &url]).output().expect("svn CLI");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        match &self.host {
            Host::Docker { container, .. } => {
                let _ = Command::new("docker")
                    .args([
                        "exec",
                        container,
                        "rm",
                        "-rf",
                        &format!("/var/svn/repos/{}", self.name),
                    ])
                    .output();
            }
            Host::Local { repos, .. } => {
                let _ = std::fs::remove_dir_all(repos.join(&self.name));
            }
        }
    }
}

#[derive(Default)]
struct Journal(Mutex<Vec<String>>);

impl CommitObserver for Journal {
    fn on_txn_created(&self, txn: &str) -> Result<()> {
        self.0.lock().unwrap().push(format!("txn:{txn}"));
        Ok(())
    }
    fn on_merged(&self, info: &CommitInfo) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .push(format!("merged:{}", info.revision));
        Ok(())
    }
}

fn file(text: &[u8]) -> (NamedTempFile, ReplayableSource, Checksum) {
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(text).unwrap();
    let md5: [u8; 16] = Md5::digest(text).into();
    let src = ReplayableSource {
        path: f.path().to_path_buf(),
        translated: true,
        size: Some(text.len() as u64),
    };
    (f, src, Checksum::Md5(md5))
}

fn prop(name: &str, value: &str) -> PropChange {
    PropChange {
        name: name.into(),
        value: Some(value.to_owned().into()),
    }
}

async fn open(scratch: &Scratch) -> RaWire {
    RaWire::open(
        transport(),
        RepoUrl::parse(&scratch.url()).unwrap(),
        &CancelToken::new(),
    )
    .await
    .expect("open session")
}

async fn checkout(wire: &RaWire) -> BTreeMap<RelPath, Vec<u8>> {
    let (editor, log) = RecordingEditor::new();
    let report: svn_ra_api::ReportFn = Box::new(|r| {
        r.set_path(&RelPath::root(), Revnum(0), Depth::Infinity, true, None)?;
        r.finish()
    });
    run_update(
        wire,
        UpdateRequest {
            revision: None,
            target: RelPath::root(),
            depth: Depth::Infinity,
            mode: UpdateMode::Update,
            send_copyfrom: false,
            ignore_ancestry: false,
        },
        report,
        Box::new(editor),
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    assert!(log.violations().is_empty(), "{:?}", log.violations());
    assert!(log.is_closed());
    log.file_contents().into_iter().collect()
}

/// First commit: directories, a text file with a property, a large binary file, an empty file.
async fn first_commit(wire: &RaWire, journal: &Arc<Journal>, big: &[u8]) -> CommitInfo {
    let mut ed = start_commit(
        wire,
        "initial import",
        &[],
        &[],
        false,
        journal.clone(),
        &CancelToken::new(),
    )
    .await
    .expect("start commit");
    let root = ed.open_root(Some(Revnum(0))).await.unwrap();
    let trunk = ed.add_directory(&p("trunk"), root, None).await.unwrap();
    let (_k1, src, md5) = file(b"hello\nworld\n");
    let a = ed.add_file(&p("trunk/a.txt"), trunk, None).await.unwrap();
    ed.apply_text(a, None, src, None, md5).await.unwrap();
    ed.change_file_prop(a, prop("svn:eol-style", "native"))
        .await
        .unwrap();
    ed.change_file_prop(a, prop("custom:note", "caf\u{e9}"))
        .await
        .unwrap();
    ed.close_file(a).await.unwrap();
    let (_k2, src, md5) = file(big);
    let b = ed.add_file(&p("trunk/big.bin"), trunk, None).await.unwrap();
    ed.apply_text(b, None, src, None, md5).await.unwrap();
    ed.close_file(b).await.unwrap();
    let e = ed.add_file(&p("trunk/empty"), trunk, None).await.unwrap();
    ed.close_file(e).await.unwrap();
    ed.change_dir_prop(trunk, prop("svn:ignore", "*.o\n"))
        .await
        .unwrap();
    ed.close_directory(trunk).await.unwrap();
    ed.close_directory(root).await.unwrap();
    ed.close_edit().await.expect("merge")
}

fn big_bytes() -> Vec<u8> {
    // 3 MiB of incompressible-ish data spanning several svndiff windows.
    let mut x: u32 = 0x1234_5678;
    (0..3 * 1024 * 1024)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 24) as u8
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires svn and a server: cargo xtask compat up --no-build, or OPENSVN_COMPAT_LOCAL_*"]
async fn commit_update_roundtrip_matches_svn() {
    for (version, host) in servers() {
        let scratch = Scratch::new(&host);
        let wire = open(&scratch).await;
        let journal = Arc::new(Journal::default());
        let big = big_bytes();
        let info = first_commit(&wire, &journal, &big).await;
        assert_eq!(info.revision, Revnum(1), "{version}");
        assert_eq!(info.author.as_deref(), Some("tester"), "{version}");
        let events = journal.0.lock().unwrap().clone();
        assert!(
            events[0].starts_with("txn:") && events[1] == "merged:r1",
            "{events:?}"
        );

        // The CLI sees what we committed.
        assert_eq!(scratch.cat("trunk/a.txt"), b"hello\nworld\n", "{version}");
        assert_eq!(scratch.cat("trunk/big.bin"), big, "{version}");
        assert!(scratch.cat("trunk/empty").is_empty(), "{version}");
        let props = scratch.svn(&[
            "proplist",
            "-v",
            &format!("{}/trunk/a.txt", scratch.cli_url()),
        ]);
        assert!(
            props.contains("svn:eol-style") && props.contains("native"),
            "{props}"
        );
        assert!(props.contains("caf\u{e9}"), "{props}");
        let log = scratch.svn(&["log", "-v", "-r1", &scratch.cli_url()]);
        assert!(
            log.contains("initial import") && log.contains("A /trunk/big.bin"),
            "{log}"
        );

        // Second commit: modify, delete, copy a directory, change a directory property.
        let mut ed = start_commit(
            &wire,
            "edit",
            &[],
            &[],
            false,
            journal.clone(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
        let root = ed.open_root(Some(Revnum(1))).await.unwrap();
        let trunk = ed
            .open_directory(&p("trunk"), root, Some(Revnum(1)))
            .await
            .unwrap();
        let (_k, base, base_md5) = file(b"hello\nworld\n");
        let (_k2, src, md5) = file(b"hello\nthere\nworld\n");
        let a = ed
            .open_file(&p("trunk/a.txt"), trunk, Some(Revnum(1)))
            .await
            .unwrap();
        ed.apply_text(a, Some(base), src, Some(base_md5), md5)
            .await
            .unwrap();
        ed.close_file(a).await.unwrap();
        ed.delete_entry(&p("trunk/big.bin"), Some(Revnum(1)), trunk)
            .await
            .unwrap();
        ed.close_directory(trunk).await.unwrap();
        let copy_src = format!("{}/trunk", scratch.url());
        let br = ed
            .add_directory(&p("branch"), root, Some((&copy_src, Revnum(1))))
            .await
            .unwrap();
        ed.close_directory(br).await.unwrap();
        ed.close_directory(root).await.unwrap();
        let info = ed.close_edit().await.unwrap();
        assert_eq!(info.revision, Revnum(2), "{version}");
        assert_eq!(scratch.cat("trunk/a.txt"), b"hello\nthere\nworld\n");
        let log = scratch.svn(&["log", "-v", "-r2", &scratch.cli_url()]);
        assert!(
            log.contains("A /branch (from /trunk:1)"),
            "{version}: {log}"
        );
        assert!(log.contains("D /trunk/big.bin"), "{log}");

        // Checkout through both update modes equals `svn cat` of every file.
        for pref in [BulkPreference::Bulk, BulkPreference::Skelta] {
            let w = wire.clone().with_bulk_preference(pref);
            let tree = checkout(&w).await;
            assert_eq!(
                tree.len(),
                5,
                "{version} {pref:?}: {:?}",
                tree.keys().collect::<Vec<_>>()
            );
            for (path, content) in &tree {
                assert_eq!(
                    content,
                    &scratch.cat(path.as_str()),
                    "{version} {pref:?} {path}"
                );
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires svn and a server: cargo xtask compat up --no-build, or OPENSVN_COMPAT_LOCAL_*"]
async fn failures_clean_up_and_map_to_error_codes() {
    for (version, host) in servers() {
        let scratch = Scratch::new(&host);
        let wire = open(&scratch).await;
        let journal = Arc::new(Journal::default());
        first_commit(&wire, &journal, b"small").await;

        // Abort leaves nothing behind and is repeatable by name.
        let mut ed = start_commit(
            &wire,
            "never",
            &[],
            &[],
            false,
            journal.clone(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
        let root = ed.open_root(Some(Revnum(1))).await.unwrap();
        let d = ed.add_directory(&p("scrap"), root, None).await.unwrap();
        ed.close_directory(d).await.unwrap();
        ed.abort_edit().await.unwrap();
        let txn = journal
            .0
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|e| e.strip_prefix("txn:").map(str::to_owned))
            .unwrap();
        abort_txn(&wire, &txn).await.unwrap();
        assert!(!scratch.svn(&["ls", &scratch.cli_url()]).contains("scrap"));

        // Out of date: change a.txt on top of a stale base.
        let mut other = start_commit(
            &wire,
            "winner",
            &[],
            &[],
            false,
            journal.clone(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
        let root = other.open_root(Some(Revnum(1))).await.unwrap();
        let trunk = other
            .open_directory(&p("trunk"), root, Some(Revnum(1)))
            .await
            .unwrap();
        let (_k, src, md5) = file(b"winner\n");
        let a = other
            .open_file(&p("trunk/a.txt"), trunk, Some(Revnum(1)))
            .await
            .unwrap();
        other.apply_text(a, None, src, None, md5).await.unwrap();
        other.close_file(a).await.unwrap();
        other.close_directory(trunk).await.unwrap();
        other.close_directory(root).await.unwrap();
        other.close_edit().await.unwrap();

        let mut stale = start_commit(
            &wire,
            "loser",
            &[],
            &[],
            false,
            journal.clone(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
        let root = stale.open_root(Some(Revnum(1))).await.unwrap();
        let trunk = stale
            .open_directory(&p("trunk"), root, Some(Revnum(1)))
            .await
            .unwrap();
        let (_k, src, md5) = file(b"loser\n");
        let a = stale
            .open_file(&p("trunk/a.txt"), trunk, Some(Revnum(1)))
            .await
            .unwrap();
        let outcome = async {
            stale.apply_text(a, None, src, None, md5).await?;
            stale.close_file(a).await?;
            stale.close_directory(trunk).await?;
            stale.close_directory(root).await
        }
        .await;
        let err = match outcome {
            Err(e) => {
                stale.abort_edit().await.unwrap();
                e
            }
            Ok(()) => stale.close_edit().await.unwrap_err(),
        };
        assert_eq!(err.code, ErrorCode::OutOfDate, "{version}: {err}");
        assert_eq!(scratch.cat("trunk/a.txt"), b"winner\n");

        // Locked by someone else.
        scratch.svn(&[
            "lock",
            &format!("{}/trunk/empty", scratch.cli_url()),
            "-m",
            "mine",
        ]);
        let mut ed = start_commit(
            &wire,
            "locked",
            &[],
            &[],
            false,
            journal.clone(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
        let root = ed.open_root(Some(Revnum(2))).await.unwrap();
        let trunk = ed
            .open_directory(&p("trunk"), root, Some(Revnum(2)))
            .await
            .unwrap();
        let (_k, src, md5) = file(b"x");
        let f = ed
            .open_file(&p("trunk/empty"), trunk, Some(Revnum(2)))
            .await
            .unwrap();
        ed.apply_text(f, None, src, None, md5).await.unwrap();
        let err = ed.close_file(f).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::LockedByOther, "{version}: {err}");
        ed.abort_edit().await.unwrap();
    }
}

/// Lock token of `path` as `svn info` reports it, if the node is locked.
fn lock_token(scratch: &Scratch, path: &str) -> Option<String> {
    let url = format!("{}/{path}", scratch.cli_url());
    scratch
        .svn(&["info", &url])
        .lines()
        .find_map(|l| l.strip_prefix("Lock Token: "))
        .map(str::to_owned)
}

/// Replacing directories whose old children are locked (`svn rm d; mkdir d; ...; svn ci`, and
/// the same with a copy): mod_dav still sees the old locks below the new directory, so `MKCOL`
/// and the directory `COPY` must present them (ra_serf's recursive `If:` header). The svn CLI
/// makes the same commit with DELETE, MKCOL, PUT and MERGE.
#[tokio::test]
#[ignore = "requires svn and a server: cargo xtask compat up --no-build, or OPENSVN_COMPAT_LOCAL_*"]
async fn replacing_directories_with_locked_children() {
    for (version, host) in servers() {
        for keep_locks in [false, true] {
            let scratch = Scratch::new(&host);
            let wire = open(&scratch).await;
            let journal = Arc::new(Journal::default());

            // r1: rd/f.txt and cd/g.txt.
            let mut ed = start_commit(
                &wire,
                "seed",
                &[],
                &[],
                false,
                journal.clone(),
                &CancelToken::new(),
            )
            .await
            .unwrap();
            let root = ed.open_root(Some(Revnum(0))).await.unwrap();
            for (dir, name) in [("rd", "f.txt"), ("cd", "g.txt")] {
                let d = ed.add_directory(&p(dir), root, None).await.unwrap();
                let (_k, src, md5) = file(b"old\n");
                let f = ed
                    .add_file(&p(&format!("{dir}/{name}")), d, None)
                    .await
                    .unwrap();
                ed.apply_text(f, None, src, None, md5).await.unwrap();
                ed.close_file(f).await.unwrap();
                ed.close_directory(d).await.unwrap();
            }
            ed.close_directory(root).await.unwrap();
            let r1 = ed.close_edit().await.unwrap().revision;

            let urls: Vec<String> = ["rd/f.txt", "cd/g.txt"]
                .iter()
                .map(|f| format!("{}/{f}", scratch.cli_url()))
                .collect();
            scratch.svn(&["lock", "-m", "held", &urls[0], &urls[1]]);
            let locks = vec![
                (
                    "rd/f.txt".to_owned(),
                    lock_token(&scratch, "rd/f.txt").unwrap(),
                ),
                (
                    "cd/g.txt".to_owned(),
                    lock_token(&scratch, "cd/g.txt").unwrap(),
                ),
            ];

            // r2: replace rd by a new directory with a new f.txt, and cd by a copy of rd@r1
            // (URL form in one run, repository-path form in the other).
            let copy_src = if keep_locks {
                "/rd".to_owned()
            } else {
                format!("{}/rd", scratch.url())
            };
            let mut ed = start_commit(
                &wire,
                "replace",
                &[],
                &locks,
                keep_locks,
                journal.clone(),
                &CancelToken::new(),
            )
            .await
            .unwrap();
            let root = ed.open_root(Some(r1)).await.unwrap();
            ed.delete_entry(&p("rd"), Some(r1), root).await.unwrap();
            let rd = ed.add_directory(&p("rd"), root, None).await.unwrap();
            let (_k, src, md5) = file(b"new\n");
            let f = ed.add_file(&p("rd/f.txt"), rd, None).await.unwrap();
            ed.apply_text(f, None, src, None, md5).await.unwrap();
            ed.close_file(f).await.unwrap();
            ed.close_directory(rd).await.unwrap();
            ed.delete_entry(&p("cd"), Some(r1), root).await.unwrap();
            let cd = ed
                .add_directory(&p("cd"), root, Some((&copy_src, r1)))
                .await
                .unwrap_or_else(|e| panic!("{version} keep={keep_locks}: copy: {e}"));
            ed.close_directory(cd).await.unwrap();
            ed.close_directory(root).await.unwrap();
            let info = ed
                .close_edit()
                .await
                .unwrap_or_else(|e| panic!("{version} keep={keep_locks}: {e}"));
            assert_eq!(info.revision, Revnum(r1.0 + 1), "{version}");

            assert_eq!(scratch.cat("rd/f.txt"), b"new\n", "{version}");
            assert_eq!(scratch.cat("cd/f.txt"), b"old\n", "{version}");
            let listing = scratch.svn(&["ls", &format!("{}/cd", scratch.cli_url())]);
            assert_eq!(listing.trim(), "f.txt", "{version}");
            // The MERGE keeps or releases the lock the commit used.
            assert_eq!(
                lock_token(&scratch, "rd/f.txt").is_some(),
                keep_locks,
                "{version} keep={keep_locks}"
            );
        }
    }
}
