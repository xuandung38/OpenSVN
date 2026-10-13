//! Write-path compatibility against the pinned Subversion servers (1.14.5 on :8443, 1.15.0 on
//! :8444) and the `svn` command line as oracle.
//!
//! Needs Docker and the `svn` CLI: `cargo xtask compat up --no-build`, then
//! `cargo test -p svn-ra --test compat_write -- --ignored`. Each run creates a scratch
//! repository on the server and removes it again, so the fixture repositories stay untouched.
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

fn p(s: &str) -> RelPath {
    RelPath::new(s).expect("path")
}

fn ca_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/compat/certs/ca.crt")
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
    container: &'static str,
    name: String,
    port: u16,
}

impl Scratch {
    fn new(container: &'static str, port: u16) -> Self {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let name = format!(
            "ra10b-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        );
        // The server runs Apache as uid 33; the repository must belong to that user.
        let out = Command::new("docker")
            .args(["exec", "-u", "33:33", container, "svnadmin", "create"])
            .arg(format!("/var/svn/repos/{name}"))
            .output()
            .expect("docker");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Self {
            container,
            name,
            port,
        }
    }

    fn url(&self) -> String {
        format!("https://localhost:{}/svn/{}", self.port, self.name)
    }

    /// URL for the CLI: Docker publishes the port on IPv4 only, and `svn` tries `::1` first.
    fn cli_url(&self) -> String {
        format!("https://127.0.0.1:{}/svn/{}", self.port, self.name)
    }

    fn cli(&self) -> Command {
        let mut c = Command::new("svn");
        c.args([
            "--non-interactive",
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
        let _ = Command::new("docker")
            .args([
                "exec",
                self.container,
                "rm",
                "-rf",
                &format!("/var/svn/repos/{}", self.name),
            ])
            .output();
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
#[ignore = "requires Docker and svn: cargo xtask compat up --no-build"]
async fn commit_update_roundtrip_matches_svn() {
    for (version, port, container) in SERVERS {
        let scratch = Scratch::new(container, port);
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
#[ignore = "requires Docker and svn: cargo xtask compat up --no-build"]
async fn failures_clean_up_and_map_to_error_codes() {
    for (version, port, container) in SERVERS {
        let scratch = Scratch::new(container, port);
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
