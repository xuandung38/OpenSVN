//! Commit timing against a local Apache + mod_dav_svn (`scripts/test-env/visualsvn-like.sh`):
//! commits of 1, 100 and 1000 new files, with the number of connections and full TLS
//! handshakes each commit opened, read from the server's access log (`port=` and `tls=`).
//!
//! ```sh
//! eval "$(scripts/test-env/visualsvn-like.sh env)"
//! OPENSVN_TIMING_URLS=https://svn.opensvn.test:18443/svn-basic,https://svn.opensvn.test:18444/svn-basic \
//!   cargo test --release -p svn-ra --test commit_timing -- --ignored --nocapture
//! ```
//!
//! `OPENSVN_TIMING_RUNS` (default 3) sets the runs per size; the median is printed.
//! `OPENSVN_TIMING_FILES` (default `1,100,1000`) the sizes, `OPENSVN_TIMING_WRITE_REQUESTS`
//! [`TransportConfig::write_connection_requests`] (default: the transport's default).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::HashSet;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use md5::{Digest, Md5};
use svn_ra::ops::commit::start_commit;
use svn_ra::ops::update::RaWire;
use svn_ra::transport::{HttpTransport, MemoryTrustStore, ProxyConfig, TransportConfig};
use svn_ra_api::{
    AuthMethod, AuthPrompter, CertDecision, CertInfo, CommitInfo, CommitObserver,
    CredentialRequest, Credentials, ReplayableSource,
};
use svn_types::{CancelToken, Checksum, RelPath, RepoUrl, Result, Revnum};
use tempfile::NamedTempFile;

struct Tester;

#[async_trait::async_trait]
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

struct NoJournal;

impl CommitObserver for NoJournal {
    fn on_txn_created(&self, _txn: &str) -> Result<()> {
        Ok(())
    }
    fn on_merged(&self, _info: &CommitInfo) -> Result<()> {
        Ok(())
    }
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set (visualsvn-like.sh env)"))
}

fn transport() -> Arc<HttpTransport> {
    let pem = std::fs::read_to_string(env("OPENSVN_COMPAT_LOCAL_CA")).unwrap();
    let b64: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .unwrap();
    let mut config = TransportConfig {
        proxy: ProxyConfig::Disabled,
        extra_root_certs: vec![der.into()],
        ..TransportConfig::default()
    };
    if let Some(n) = std::env::var("OPENSVN_TIMING_WRITE_REQUESTS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        config.write_connection_requests = n;
    }
    Arc::new(HttpTransport::new(
        config,
        Arc::new(Tester),
        Arc::new(MemoryTrustStore::new()),
    ))
}

/// A scratch repository below `OPENSVN_COMPAT_LOCAL_REPOS`, deleted when dropped.
struct Scratch {
    dir: PathBuf,
    name: String,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let name = format!("timing-{tag}-{}", std::process::id());
        let dir = PathBuf::from(env("OPENSVN_COMPAT_LOCAL_REPOS")).join(&name);
        let _ = std::fs::remove_dir_all(&dir);
        let svnadmin =
            std::env::var("OPENSVN_COMPAT_LOCAL_SVNADMIN").unwrap_or_else(|_| "svnadmin".into());
        let out = Command::new(svnadmin)
            .arg("create")
            .arg(&dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        Self { dir, name }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The access log of the server behind `url` (`access-14.log` for :18443, `-15` for :18444).
fn access_log(url: &str) -> PathBuf {
    let logs = PathBuf::from(env("OPENSVN_COMPAT_LOCAL_CA"))
        .parent()
        .and_then(|p| p.parent())
        .unwrap()
        .join("logs");
    let port = std::env::var("OPENSVN_VSVN_HTTPS115_PORT").unwrap_or_else(|_| "18444".into());
    let tag = if url.contains(&format!(":{port}/")) {
        "15"
    } else {
        "14"
    };
    logs.join(format!("access-{tag}.log"))
}

/// Connections and full handshakes among the log lines after `mark` that name `repo`.
fn connections(log: &PathBuf, mark: usize, repo: &str) -> (usize, usize, usize) {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let mut ports = HashSet::new();
    let mut initial = HashSet::new();
    let mut requests = 0;
    for l in text.lines().skip(mark).filter(|l| l.contains(repo)) {
        requests += 1;
        let port = l.split(" port=").nth(1).and_then(|r| r.split(' ').next());
        let Some(port) = port else { continue };
        ports.insert(port.to_owned());
        if l.ends_with("tls=Initial") {
            initial.insert(port.to_owned());
        }
    }
    (requests, ports.len(), initial.len())
}

fn line_count(log: &PathBuf) -> usize {
    std::fs::read_to_string(log)
        .map(|t| t.lines().count())
        .unwrap_or(0)
}

fn source(text: &[u8]) -> (NamedTempFile, ReplayableSource, Checksum) {
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

/// Commits `files` new files below a new directory `dir`; returns the wall time of the commit
/// (transaction creation to the merged revision).
async fn commit_files(wire: &RaWire, base: Revnum, dir: &str, files: usize) -> Duration {
    let texts: Vec<_> = (0..files)
        .map(|i| source(format!("file {i} of {dir}\n").as_bytes()))
        .collect();
    let started = Instant::now();
    let mut ed = start_commit(
        wire,
        "timing",
        &[],
        &[],
        false,
        Arc::new(NoJournal),
        &CancelToken::new(),
    )
    .await
    .expect("start commit");
    let root = ed.open_root(Some(base)).await.unwrap();
    let d = ed
        .add_directory(&RelPath::new(dir).unwrap(), root, None)
        .await
        .unwrap();
    for (i, (_keep, src, md5)) in texts.iter().enumerate() {
        let f = ed
            .add_file(
                &RelPath::new(&format!("{dir}/f{i:05}.txt")).unwrap(),
                d,
                None,
            )
            .await
            .unwrap();
        ed.apply_text(f, None, src.clone(), None, *md5)
            .await
            .unwrap();
        ed.close_file(f).await.unwrap();
    }
    ed.close_directory(d).await.unwrap();
    ed.close_directory(root).await.unwrap();
    ed.close_edit().await.expect("commit");
    started.elapsed()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs scripts/test-env/visualsvn-like.sh"]
async fn commit_timing() {
    let urls = env("OPENSVN_TIMING_URLS");
    let runs: usize = std::env::var("OPENSVN_TIMING_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let sizes: Vec<usize> = std::env::var("OPENSVN_TIMING_FILES")
        .unwrap_or_else(|_| "1,100,1000".into())
        .split(',')
        .map(|s| s.trim().parse().unwrap())
        .collect();
    println!("| server | files | median s | requests | connections | full handshakes |");
    println!("|---|---|---|---|---|---|");
    for base in urls.split(',') {
        let base = base.trim().trim_end_matches('/');
        let log = access_log(base);
        for &n in &sizes {
            let mut times = Vec::new();
            let mut last = (0, 0, 0);
            for run in 0..runs {
                let scratch = Scratch::new(&format!("{n}-{run}"));
                let url = format!("{base}/{}", scratch.name);
                let t = transport();
                let wire = RaWire::open(t, RepoUrl::parse(&url).unwrap(), &CancelToken::new())
                    .await
                    .expect("open session");
                let mark = line_count(&log);
                times.push(commit_files(&wire, Revnum(0), "d", n).await);
                // Apache writes the log line once the response went out.
                tokio::time::sleep(Duration::from_millis(200)).await;
                last = connections(&log, mark, &scratch.name);
            }
            times.sort();
            let median = times[times.len() / 2];
            println!(
                "| {base} | {n} | {:.3} | {} | {} | {} |",
                median.as_secs_f64(),
                last.0,
                last.1,
                last.2
            );
        }
    }
}
