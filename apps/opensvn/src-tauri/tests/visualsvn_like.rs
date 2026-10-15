//! The real engine against a VisualSVN-like server: Apache + mod_dav_svn + mod_auth_gssapi with a
//! local Kerberos realm, `SVNParentPath` with several repositories, VisualSVN-style authz and
//! hooks. `scripts/test-env/visualsvn-like.sh up` builds that server (see
//! docs/visualsvn-test-env.md); every test is `#[ignore]`:
//!
//! ```text
//! eval "$(scripts/test-env/visualsvn-like.sh env)"
//! cargo test -p opensvn --features real-engine,negotiate-gssapi,engine-conformance \
//!   --test visualsvn_like -- --ignored
//! ```
//!
//! `OPENSVN_VSVN_HTTPS_PORT` selects the server (18443: distribution mod_dav_svn, 18444: the
//! self-built 1.15.0). The process must hold a ticket for `alice` (`KRB5CCNAME`); the
//! no-ticket cases run themselves again in a child process with an empty credential cache.
//!
//! Large-content cases (`big_*`) are slow; `OPENSVN_VSVN_BIG_MB` (default 120) and
//! `OPENSVN_VSVN_MANY_FILES` (default 10000) size them.
#![cfg(all(feature = "real-engine", feature = "engine-conformance"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use opensvn_ipc::{CommitRequest, Depth, UpdateRequest, WcFormat, WcId};
use opensvn_lib::engine::real::{ClientRecovery, HttpConnector, RealEngine, RealEngineConfig};
use opensvn_lib::engine::{
    CheckoutPlan, EditOp, EngineFacade, LockScope, LogScope, OpContext, RemoteEditPlan,
    StatusQuery, WcRef,
};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject as _;
use svn_ra::transport::{MemoryTrustStore, ProxyConfig};
use svn_ra_api::{
    AuthMethod, AuthPrompter, CertDecision, CertInfo, CredentialRequest, Credentials, PromptReason,
};
use svn_types::{ErrorCode, LockOwnershipStore, MemoryLockOwnership, SvnError};
use tokio::sync::watch;

const BOOT: &str = "boot-now";

fn var(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn host() -> String {
    var("OPENSVN_VSVN_HOST", "svn.opensvn.test")
}

/// `https://<host>:<port>/<location>` of the server under test.
fn base(location: &str) -> String {
    format!(
        "https://{}:{}/{location}",
        host(),
        var("OPENSVN_VSVN_HTTPS_PORT", "18443")
    )
}

fn repos_dir() -> PathBuf {
    PathBuf::from(var(
        "OPENSVN_COMPAT_LOCAL_REPOS",
        "/tmp/opensvn-visualsvn/repos",
    ))
}

fn svnadmin() -> Command {
    let mut c = Command::new(var("OPENSVN_COMPAT_LOCAL_SVNADMIN", "svnadmin"));
    c.env("LANG", "C.UTF-8");
    c
}

/// What the prompter saw and answers.
#[derive(Default)]
struct Prompts {
    seen: Mutex<Vec<CredentialRequest>>,
    /// Basic account to answer with; `None` refuses every prompt.
    answer: Option<(&'static str, &'static str)>,
}

#[async_trait]
impl AuthPrompter for Prompts {
    async fn credentials(&self, req: &CredentialRequest) -> Option<Credentials> {
        self.seen.lock().unwrap().push(req.clone());
        let (user, pass) = self.answer?;
        Some(Credentials {
            username: user.into(),
            password: secrecy::SecretString::from(pass.to_owned()),
            save: false,
            method: AuthMethod::Basic,
        })
    }
    async fn confirm_certificate(&self, _info: &CertInfo) -> CertDecision {
        CertDecision::Reject
    }
}

impl Prompts {
    fn seen(&self) -> Vec<CredentialRequest> {
        self.seen.lock().unwrap().clone()
    }
}

struct Env {
    engine: RealEngine,
    prompts: Arc<Prompts>,
    tmp: tempfile::TempDir,
}

fn env_with(answer: Option<(&'static str, &'static str)>) -> Env {
    let ca = var(
        "OPENSVN_COMPAT_LOCAL_CA",
        "/tmp/opensvn-visualsvn/certs/ca.crt",
    );
    let der = CertificateDer::from_pem_file(&ca).unwrap_or_else(|e| panic!("test CA {ca}: {e}"));
    let prompts = Arc::new(Prompts {
        seen: Mutex::default(),
        answer,
    });
    let (_settings, rx) = watch::channel(opensvn_lib::settings::defaults(None));
    let (_invalidate, inv) = watch::channel(0u64);
    let connector = Arc::new(HttpConnector::with_config_tweak(
        prompts.clone() as Arc<dyn AuthPrompter>,
        Arc::new(MemoryTrustStore::new()),
        rx,
        inv,
        move |c| {
            c.proxy = ProxyConfig::Disabled;
            c.extra_root_certs = vec![der.clone()];
        },
    ));
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let engine = RealEngine::new(RealEngineConfig {
        connector,
        lock_ownership: Arc::new(MemoryLockOwnership::for_current_process(BOOT))
            as Arc<dyn LockOwnershipStore>,
        boot_id: BOOT.to_owned(),
        state_dir: state,
        recovery: Arc::new(ClientRecovery),
    });
    Env {
        engine,
        prompts,
        tmp,
    }
}

/// SSO: no prompt is answered.
fn sso() -> Env {
    env_with(None)
}

fn ctx() -> OpContext {
    OpContext::detached()
}

/// A fresh repository in the parent path (removed on drop).
struct Repo {
    name: String,
}

impl Repo {
    fn new(prefix: &str) -> Self {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let name = format!(
            "{prefix}-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        );
        let out = svnadmin()
            .arg("create")
            .arg(repos_dir().join(&name))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "svnadmin create {name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Self { name }
    }
    /// URL with the name percent-encoded, below `/svn`.
    fn url(&self) -> String {
        format!("{}/{}", base("svn"), encode(&self.name))
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(repos_dir().join(&self.name));
    }
}

/// Percent-encodes a path segment (UTF-8, space and non-ASCII), as VisualSVN shows URLs.
fn encode(segment: &str) -> String {
    let mut out = String::new();
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

async fn checkout(e: &Env, url: &str, name: &str) -> WcRef {
    let info = e
        .engine
        .checkout(
            &CheckoutPlan {
                url: url.to_owned(),
                path: e.tmp.path().join(name),
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
        .unwrap_or_else(|err| panic!("checkout {url}: {err:?}"));
    WcRef {
        id: WcId(name.to_owned()),
        root: PathBuf::from(info.root_path),
    }
}

async fn commit(e: &Env, wc: &WcRef, message: &str) -> Result<opensvn_ipc::Rev, SvnError> {
    e.engine
        .commit(
            wc,
            &CommitRequest {
                wc_id: wc.id.clone(),
                paths: vec![String::new()],
                message: message.into(),
                keep_locks: false,
                auto_add_remove: true,
            },
            &ctx(),
        )
        .await
}

async fn update(e: &Env, wc: &WcRef) -> opensvn_ipc::Rev {
    e.engine
        .update(
            wc,
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
}

fn write(root: &Path, rel: &str, content: impl AsRef<[u8]>) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

/// Runs the named test again in a child process without a Kerberos ticket. Returns true in the
/// parent (the child did the work and passed), false in the child.
fn rerun_without_ticket(test: &str) -> bool {
    if std::env::var_os("OPENSVN_VSVN_CHILD").is_some() {
        return false;
    }
    let empty = tempfile::tempdir().unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            test,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("OPENSVN_VSVN_CHILD", "1")
        .env(
            "KRB5CCNAME",
            format!("FILE:{}", empty.path().join("none").display()),
        )
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success() && text.contains("1 passed"),
        "child run of {test}:\n{text}"
    );
    true
}

// ---- Kerberos single sign-on ----

/// The whole daily cycle with only the ticket: no prompt at any step.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the VisualSVN-like server (scripts/test-env/visualsvn-like.sh)"]
async fn sso_checkout_commit_lock_log_blame_without_prompt() {
    let repo = Repo::new("sso");
    let e = sso();
    let url = repo.url();
    let info = e.engine.repo_info(&url, &ctx()).await.expect("repo_info");
    assert_eq!(info.youngest_rev.0, 0);
    let wc = checkout(&e, &url, "sso").await;
    write(&wc.root, "a.txt", "one\n");
    let r1 = commit(&e, &wc, "first").await.expect("commit");
    write(&wc.root, "a.txt", "one\ntwo\n");
    let r2 = commit(&e, &wc, "second").await.expect("commit");
    assert_eq!((r1.0, r2.0), (1, 2));
    e.engine
        .lock(
            &LockScope::Wc {
                wc: wc.clone(),
                paths: vec!["a.txt".into()],
            },
            Some("SSO lock"),
            false,
            &ctx(),
        )
        .await
        .expect("lock");
    let list = e
        .engine
        .repo_list(&url, None, &ctx())
        .await
        .expect("list")
        .entries;
    let lock = list[0].lock.as_ref().expect("lock shown in the listing");
    assert_eq!(lock.owner, "alice", "{lock:?}");
    e.engine
        .unlock(
            &LockScope::Wc {
                wc: wc.clone(),
                paths: vec!["a.txt".into()],
            },
            false,
            &ctx(),
        )
        .await
        .expect("unlock");
    let blame = e
        .engine
        .blame(&format!("{url}/a.txt"), None, &ctx())
        .await
        .expect("blame");
    assert_eq!(blame.len(), 2);
    assert_eq!(blame[1].author.as_deref(), Some("alice"));
    let log = e
        .engine
        .log(
            &LogScope::Url(url.clone()),
            &opensvn_ipc::LogGetRequest {
                target: opensvn_ipc::LogTarget::Url { url: url.clone() },
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
        .expect("log");
    assert_eq!(log.entries.len(), 2);
    assert!(e.prompts.seen().is_empty(), "{:?}", e.prompts.seen());
}

/// No ticket, server offers Negotiate and Basic: the user gets the sign-in prompt with reason
/// `NegotiateFailed` and Basic (checked against the KDC by the server) works.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the VisualSVN-like server (scripts/test-env/visualsvn-like.sh)"]
async fn no_ticket_falls_back_to_basic_prompt() {
    if rerun_without_ticket("no_ticket_falls_back_to_basic_prompt") {
        return;
    }
    let repo = Repo::new("noticket");
    let e = env_with(Some(("alice", "alice-pass")));
    let info = e
        .engine
        .repo_info(&repo.url(), &ctx())
        .await
        .expect("Basic after the prompt");
    assert_eq!(info.youngest_rev.0, 0);
    let seen = e.prompts.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].reason, PromptReason::NegotiateFailed, "{seen:?}");
    assert!(seen[0].offered.contains(&AuthMethod::Basic), "{seen:?}");
}

/// No ticket, server offers Negotiate only: `NegotiateFailed`, and no prompt (nothing to type).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the VisualSVN-like server (scripts/test-env/visualsvn-like.sh)"]
async fn no_ticket_on_negotiate_only_server_fails_with_negotiate_failed() {
    if rerun_without_ticket("no_ticket_on_negotiate_only_server_fails_with_negotiate_failed") {
        return;
    }
    let repo = Repo::new("noticket-sso");
    let e = env_with(Some(("alice", "alice-pass")));
    let url = format!("{}/{}", base("svn-sso"), repo.name);
    let err = e
        .engine
        .repo_info(&url, &ctx())
        .await
        .expect_err("no ticket, no Basic");
    assert_eq!(err.code, ErrorCode::NegotiateFailed, "{err:?}");
    assert!(e.prompts.seen().is_empty(), "{:?}", e.prompts.seen());
}

/// A short host name (`https://svnhost/`) or an IP address has no service principal: even with
/// a ticket, single sign-on fails and the user gets the Basic prompt (reason `NegotiateFailed`);
/// the AD password then works. On Negotiate-only locations the error is `NegotiateFailed`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the VisualSVN-like server (scripts/test-env/visualsvn-like.sh)"]
async fn short_host_name_and_ip_fall_back_to_the_basic_prompt() {
    let repo = Repo::new("short");
    let port = var("OPENSVN_VSVN_HTTPS_PORT", "18443");
    for h in [
        var("OPENSVN_VSVN_SHORT_HOST", "svnhost"),
        "127.0.0.1".to_owned(),
    ] {
        let e = env_with(Some(("alice", "alice-pass")));
        let url = format!("https://{h}:{port}/svn/{}", repo.name);
        let info = e
            .engine
            .repo_info(&url, &ctx())
            .await
            .unwrap_or_else(|err| panic!("{url}: {err:?}"));
        assert_eq!(info.youngest_rev.0, 0);
        let seen = e.prompts.seen();
        assert_eq!(seen.len(), 1, "{url}: {seen:?}");
        assert_eq!(
            seen[0].reason,
            PromptReason::NegotiateFailed,
            "{url}: {seen:?}"
        );

        let sso_only = format!("https://{h}:{port}/svn-sso/{}", repo.name);
        let err = env_with(Some(("alice", "alice-pass")))
            .engine
            .repo_info(&sso_only, &ctx())
            .await
            .expect_err("no SPN, no Basic");
        assert_eq!(err.code, ErrorCode::NegotiateFailed, "{sso_only}: {err:?}");
    }
}

/// Plain http with Negotiate offered: refused before any token is sent.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the VisualSVN-like server (scripts/test-env/visualsvn-like.sh)"]
async fn http_url_with_negotiate_is_refused() {
    let repo = Repo::new("plain-http");
    let e = env_with(Some(("alice", "alice-pass")));
    let url = format!(
        "http://{}:{}/svn/{}",
        host(),
        var("OPENSVN_VSVN_HTTP_PORT", "18080"),
        repo.name
    );
    let err = e
        .engine
        .repo_info(&url, &ctx())
        .await
        .expect_err("http is refused");
    assert_eq!(err.code, ErrorCode::NegotiateFailed, "{err:?}");
    assert!(
        format!("{err:?}").contains("https"),
        "the error names https: {err:?}"
    );
    assert!(e.prompts.seen().is_empty(), "{:?}", e.prompts.seen());
}

// ---- VisualSVN layout ----

/// Repository names with spaces and Vietnamese characters, typed raw or percent-encoded.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the VisualSVN-like server (scripts/test-env/visualsvn-like.sh)"]
async fn repository_names_with_spaces_and_vietnamese() {
    let e = sso();
    for name in ["My Project", "Dự án Tiếng Việt"] {
        for url in [
            format!("{}/{}", base("svn"), encode(name)),
            format!("{}/{name}", base("svn")),
        ] {
            let info = e
                .engine
                .repo_info(&url, &ctx())
                .await
                .unwrap_or_else(|err| panic!("repo_info {url}: {err:?}"));
            assert!(
                info.repo_root_url.ends_with(&encode(name)) || info.repo_root_url.ends_with(name),
                "{url} -> {}",
                info.repo_root_url
            );
        }
        let url = format!("{}/{}", base("svn"), encode(name));
        let wc = checkout(&e, &url, &format!("wc-{}", encode(name))).await;
        let file = format!("Tài liệu/ghi chú #{} (bản nháp).txt", std::process::id());
        let file = file.as_str();
        write(&wc.root, file, format!("{name}\n"));
        let rev = commit(&e, &wc, "Thêm tài liệu").await.expect("commit");
        let text = e
            .engine
            .repo_cat(&format!("{url}/{}", encode_path(file)), Some(rev), &ctx())
            .await
            .expect("cat");
        assert_eq!(text.content, format!("{name}\n"));
        let wc2 = checkout(&e, &url, &format!("wc2-{}", encode(name))).await;
        assert_eq!(
            std::fs::read_to_string(wc2.root.join(file)).unwrap(),
            format!("{name}\n")
        );
    }
}

/// A VisualSVN web-interface link (`https://host/!/#Repo/view/...`) converts to the `/svn/<Repo>`
/// URL the server really serves, for names with dots, spaces and Vietnamese.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the VisualSVN-like server (scripts/test-env/visualsvn-like.sh)"]
async fn web_interface_links_convert_to_repository_urls() {
    let e = sso();
    let dotted = Repo::new("web.link.repo");
    for name in ["My Project", "Dự án Tiếng Việt", dotted.name.as_str()] {
        for (link, rev) in [
            (format!("{}#{name}", base("!/")), None),
            (format!("{}#{}/view/head/", base("!/"), encode(name)), None),
            (format!("{}#{name}/view/r0/", base("!/")), Some(0)),
        ] {
            let parsed = svn_types::parse_visualsvn_web_link(&link)
                .unwrap_or_else(|| panic!("{link} is a web link"))
                .unwrap_or_else(|err| panic!("{link}: {err}"));
            let url = parsed.repo_url.to_string();
            assert_eq!(url, format!("{}/{}", base("svn"), encode(name)), "{link}");
            assert_eq!(parsed.peg_rev.map(|r| r.0), rev, "{link}");
            let info = e
                .engine
                .repo_info(&url, &ctx())
                .await
                .unwrap_or_else(|err| panic!("repo_info {url}: {err:?}"));
            assert!(info.repo_root_url.ends_with(&encode(name)), "{link}");
        }
    }
    // The web UI itself is not a repository: the raw link is still refused with the hint.
    let err = e
        .engine
        .repo_info(&format!("{}#{}", base("!/"), dotted.name), &ctx())
        .await
        .expect_err("raw web link");
    assert!(err.message.contains("Copy URL to clipboard"), "{err:?}");
}

/// An `r<N>` web link names the path as it was at N: a folder deleted after N is still found
/// there (peg revision), and checking it out at N gives its content at N.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the VisualSVN-like server (scripts/test-env/visualsvn-like.sh)"]
async fn web_link_to_an_old_revision_finds_a_path_deleted_since() {
    let e = sso();
    let repo = Repo::new("web-link-rev");
    let wc = checkout(&e, &repo.url(), "web-link-rev").await;
    write(&wc.root, "trunk/gone/a.txt", "kept at r1\n");
    let r1 = commit(&e, &wc, "add gone").await.expect("commit r1");
    let gone = format!("{}/trunk/gone", repo.url());
    e.engine
        .remote_edit(
            &RemoteEditPlan {
                ops: vec![EditOp::Delete { url: gone.clone() }],
                message: "delete gone".into(),
            },
            &ctx(),
        )
        .await
        .expect("delete");

    let link = format!("{}#{}/view/r{}/trunk/gone/", base("!/"), repo.name, r1.0);
    let parsed = svn_types::parse_visualsvn_web_link(&link)
        .expect("web link")
        .expect("parses");
    assert_eq!(parsed.repo_url.to_string(), gone);
    assert_eq!(parsed.peg_rev.map(|r| r.0), Some(r1.0));
    let peg = parsed.peg_rev.map(|r| opensvn_ipc::Rev(r.0));

    // At HEAD the folder is gone; at the link's revision it is there.
    let head = e.engine.repo_info(&gone, &ctx()).await.expect_err("HEAD");
    assert_eq!(head.code, ErrorCode::PathNotFound, "{head:?}");
    let info = e
        .engine
        .repo_info_at(&gone, peg, &ctx())
        .await
        .expect("repo_info at the link revision");
    assert_eq!(info.kind, opensvn_ipc::EntryKind::Dir);
    assert_eq!(info.youngest_rev.0, r1.0 + 1);

    // A checkout of `gone@r1` at r1, as the checkout dialog sends it.
    let old = e
        .engine
        .checkout(
            &CheckoutPlan {
                url: gone.clone(),
                path: e.tmp.path().join("gone-at-r1"),
                rev: peg,
                depth: Depth::Infinity,
                sparse: None,
                format: WcFormat::V31,
                store_pristine: true,
                ignore_externals: false,
            },
            &ctx(),
        )
        .await
        .unwrap_or_else(|err| panic!("checkout {gone}@{}: {err:?}", r1.0));
    let content = std::fs::read_to_string(Path::new(&old.root_path).join("a.txt")).unwrap();
    assert_eq!(content, "kept at r1\n");
}

fn encode_path(path: &str) -> String {
    path.split('/').map(encode).collect::<Vec<_>>().join("/")
}

/// `/svn/myrepo` and `/svn/MyRepo` reach the same repository (NTFS on the VisualSVN host).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the VisualSVN-like server (scripts/test-env/visualsvn-like.sh)"]
async fn repository_name_case_variants() {
    let e = sso();
    let upper = format!("{}/MyRepo", base("svn"));
    let lower = format!("{}/myrepo", base("svn"));
    let a = e.engine.repo_info(&upper, &ctx()).await.expect("MyRepo");
    let b = e.engine.repo_info(&lower, &ctx()).await.expect("myrepo");
    assert_eq!(a.repo_uuid, b.repo_uuid);
    let wc_u = checkout(&e, &upper, "case-upper").await;
    let wc_l = checkout(&e, &lower, "case-lower").await;
    let file = format!("case-{}.txt", std::process::id());
    write(&wc_u.root, &file, "via MyRepo\n");
    let rev = commit(&e, &wc_u, "via MyRepo").await.expect("commit");
    assert_eq!(update(&e, &wc_l).await, rev);
    assert_eq!(
        std::fs::read_to_string(wc_l.root.join(&file)).unwrap(),
        "via MyRepo\n"
    );
    write(&wc_l.root, &file, "via myrepo\n");
    commit(&e, &wc_l, "via myrepo").await.expect("commit");
    // The server reports the repository root in the spelling of the request, so the two
    // working copies have different roots: switching between spellings is refused, as
    // `svn switch` refuses it (relocate instead).
    let err = e
        .engine
        .switch(&wc_l, &upper, None, &ctx())
        .await
        .expect_err("switch across spellings");
    assert_eq!(err.code, ErrorCode::WcInvalidOperation, "{err:?}");
}

/// The parent path URL (`https://host/svn/`, what VisualSVN shows as the server URL) lists
/// repositories: the app says so and names them instead of "HTTP 500".
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the VisualSVN-like server (scripts/test-env/visualsvn-like.sh)"]
async fn parent_path_url_names_the_repositories() {
    let e = sso();
    for url in [format!("{}/", base("svn")), base("svn"), base("svn-sso")] {
        let err = e
            .engine
            .repo_info(&url, &ctx())
            .await
            .expect_err("the parent path is not a repository");
        assert_eq!(err.code, ErrorCode::PathNotFound, "{url}: {err:?}");
        assert!(
            err.message.contains("list of repositories")
                && err.message.contains("MyRepo")
                && err.message.contains("Dự án Tiếng Việt"),
            "{url}: {}",
            err.message
        );
    }
    assert!(e.prompts.seen().is_empty(), "{:?}", e.prompts.seen());
}

/// The repository list of the parent path rides on the error (issue #133): every listed URL is
/// percent-encoded and opens its repository, through SSO and through Basic alike.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the VisualSVN-like server (scripts/test-env/visualsvn-like.sh)"]
async fn parent_path_url_lists_repositories_that_open() {
    let sso = sso();
    let basic = env_with(Some(("tester", "tester-pass")));
    for (e, location) in [(&sso, "svn"), (&sso, "svn-sso"), (&basic, "svn-basic")] {
        let url = format!("{}/", base(location));
        let err = e
            .engine
            .repo_list(&url, None, &ctx())
            .await
            .expect_err("the parent path is not a repository");
        assert_eq!(err.code, ErrorCode::PathNotFound, "{url}: {err:?}");
        let repos = err
            .parent_repositories()
            .unwrap_or_else(|| panic!("{url}: no repositories on {err:?}"));
        let find = |name: &str| {
            repos
                .iter()
                .find(|r| r.name == name)
                .unwrap_or_else(|| panic!("{url}: {name} missing from {repos:?}"))
        };
        for name in ["MyRepo", "My Project", "Dự án Tiếng Việt"] {
            let repo = find(name);
            assert_eq!(
                repo.url,
                format!("{}/{}", base(location), encode(name)),
                "{name}"
            );
            let info = e
                .engine
                .repo_info(&repo.url, &ctx())
                .await
                .unwrap_or_else(|err| panic!("{}: {err:?}", repo.url));
            assert_eq!(
                info.repo_root_url.trim_end_matches('/'),
                repo.url,
                "{name} opens as its own root"
            );
        }
        // Each listed URL is spelled once, without the trailing slash of the listing.
        assert!(repos.iter().all(|r| !r.url.ends_with('/')), "{repos:?}");
    }
    assert!(sso.prompts.seen().is_empty(), "{:?}", sso.prompts.seen());
    assert_eq!(basic.prompts.seen().len(), 1, "one Basic sign-in");
}

/// Without a ticket and with the prompt refused, the parent path reports the sign-in, not a
/// repository list it could not read.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the VisualSVN-like server (scripts/test-env/visualsvn-like.sh)"]
async fn parent_path_url_without_a_sign_in_reports_the_sign_in() {
    if rerun_without_ticket("parent_path_url_without_a_sign_in_reports_the_sign_in") {
        return;
    }
    let e = sso();
    let err = e
        .engine
        .repo_list(&format!("{}/", base("svn")), None, &ctx())
        .await
        .expect_err("no ticket, prompt refused");
    assert!(
        matches!(
            err.code,
            ErrorCode::AuthRequired | ErrorCode::NegotiateFailed
        ),
        "{err:?}"
    );
    assert!(err.parent_repositories().is_none(), "{err:?}");
}

/// A server certificate the user has not trusted stays a certificate error on the parent path:
/// the listing is never read past it (default deny).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the VisualSVN-like server (scripts/test-env/visualsvn-like.sh)"]
async fn parent_path_url_with_an_untrusted_certificate_stays_a_certificate_error() {
    let prompts = Arc::new(Prompts::default());
    let (_settings, rx) = watch::channel(opensvn_lib::settings::defaults(None));
    let (_invalidate, inv) = watch::channel(0u64);
    let connector = Arc::new(HttpConnector::with_config_tweak(
        prompts.clone() as Arc<dyn AuthPrompter>,
        Arc::new(MemoryTrustStore::new()),
        rx,
        inv,
        |c| c.proxy = ProxyConfig::Disabled,
    ));
    let tmp = tempfile::tempdir().unwrap();
    let engine = RealEngine::new(RealEngineConfig {
        connector,
        lock_ownership: Arc::new(MemoryLockOwnership::for_current_process(BOOT))
            as Arc<dyn LockOwnershipStore>,
        boot_id: BOOT.to_owned(),
        state_dir: tmp.path().to_owned(),
        recovery: Arc::new(ClientRecovery),
    });
    let err = engine
        .repo_list(&format!("{}/", base("svn")), None, &ctx())
        .await
        .expect_err("the test CA is not trusted here");
    assert_eq!(err.code, ErrorCode::TlsUntrusted, "{err:?}");
    assert!(err.parent_repositories().is_none(), "{err:?}");
}

/// VisualSVN-style authz (`[groups]`, global `[/]`, a repository-specific denied folder).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the VisualSVN-like server (scripts/test-env/visualsvn-like.sh)"]
async fn authz_read_only_group_and_denied_folder() {
    // bob is a reader through Basic (htpasswd at /svn-basic).
    let e = env_with(Some(("bob", "bob-pass")));
    let url = format!("{}/MyRepo", base("svn-basic"));
    // alice (SSO) creates the secret folder.
    let admin = sso();
    let wc = checkout(&admin, &format!("{}/MyRepo", base("svn")), "authz-admin").await;
    let secret = format!("secret/s-{}.txt", std::process::id());
    write(&wc.root, &secret, "top secret\n");
    write(&wc.root, "public.txt", "public\n");
    commit(&admin, &wc, "secret and public")
        .await
        .expect("commit");

    let names: Vec<_> = e
        .engine
        .repo_list(&url, None, &ctx())
        .await
        .expect("bob reads the root")
        .entries
        .into_iter()
        .map(|x| x.name)
        .collect();
    assert!(!names.contains(&"secret".to_owned()), "{names:?}");
    let err = e
        .engine
        .repo_cat(&format!("{url}/{secret}"), None, &ctx())
        .await
        .expect_err("secret denied");
    assert_eq!(err.code, ErrorCode::Forbidden, "{err:?}");
    let wc_b = checkout(&e, &url, "authz-bob").await;
    assert!(!wc_b.root.join("secret").exists());
    write(&wc_b.root, "public.txt", "bob was here\n");
    let err = commit(&e, &wc_b, "read-only user")
        .await
        .expect_err("reader");
    assert_eq!(err.code, ErrorCode::Forbidden, "{err:?}");
}

/// A pre-commit hook rejects the commit; its message reaches the user.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the VisualSVN-like server (scripts/test-env/visualsvn-like.sh)"]
async fn pre_commit_hook_message_reaches_the_user() {
    let e = sso();
    let url = format!("{}/Hooked", base("svn"));
    let wc = checkout(&e, &url, "hooked").await;
    write(&wc.root, &format!("h-{}.txt", std::process::id()), "x\n");
    let err = commit(&e, &wc, "no issue key").await.expect_err("hook");
    assert_eq!(err.code, ErrorCode::HookFailed, "{err:?}");
    let text = format!("{err}");
    let full = format!("{err:?}");
    assert!(
        full.contains("Commit rejected by policy") && full.contains("Thông điệp commit"),
        "hook output: {full}"
    );
    eprintln!("hook error shown: {text}");
    // The working copy is unchanged and the commit goes through with a key.
    commit(&e, &wc, "ISSUE-1 with key").await.expect("commit");
}

/// A failing post-commit hook does not fail the commit: the revision exists and the working
/// copy is at it (svn only prints a warning). Reporting an error here would make the user
/// commit the same change twice.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the VisualSVN-like server (scripts/test-env/visualsvn-like.sh)"]
async fn failing_post_commit_hook_keeps_the_commit() {
    let e = sso();
    let url = format!("{}/Hooked", base("svn"));
    let wc = checkout(&e, &url, "post-commit").await;
    let file = format!("p-{}.txt", std::process::id());
    write(&wc.root, &file, "x\n");
    let rev = commit(&e, &wc, "ISSUE-2 POSTFAIL")
        .await
        .expect("the commit itself succeeded");
    let info = e.engine.repo_info(&url, &ctx()).await.expect("repo_info");
    assert_eq!(info.youngest_rev, rev);
    let st = e
        .engine
        .status(
            &wc,
            StatusQuery {
                include_ignored: false,
                include_unversioned: true,
            },
            &ctx(),
        )
        .await
        .expect("status");
    assert!(st.is_empty(), "nothing left to commit: {st:?}");
}

/// A pre-lock hook refuses the lock; its message reaches the user.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the VisualSVN-like server (scripts/test-env/visualsvn-like.sh)"]
async fn pre_lock_hook_message_reaches_the_user() {
    let e = sso();
    let url = format!("{}/Hooked", base("svn"));
    let wc = checkout(&e, &url, "pre-lock").await;
    let file = format!("nolock/l-{}.bin", std::process::id());
    write(&wc.root, &file, "x\n");
    commit(&e, &wc, "ISSUE-3 lockable file")
        .await
        .expect("commit");
    let err = e
        .engine
        .lock(
            &LockScope::Wc {
                wc: wc.clone(),
                paths: vec![file.clone()],
            },
            None,
            false,
            &ctx(),
        )
        .await
        .expect_err("pre-lock refuses");
    assert!(
        format!("{err:?}").contains("not allowed by policy"),
        "hook output: {err:?}"
    );
    eprintln!("pre-lock error shown: {err}");
}

/// svn:keywords and svn:eol-style are expanded on checkout; svn:needs-lock makes files read-only
/// until locked.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the VisualSVN-like server (scripts/test-env/visualsvn-like.sh)"]
async fn keywords_eol_and_needs_lock() {
    let repo = Repo::new("props");
    let e = sso();
    let url = repo.url();
    let wc = checkout(&e, &url, "props").await;
    write(&wc.root, "kw.txt", "$Id$\n$Rev$\n$Author$\nline\n");
    write(&wc.root, "crlf.txt", "a\nb\n");
    write(&wc.root, "locked.bin", [0u8, 1, 2, 3]);
    e.engine
        .add_paths(
            &wc,
            &["kw.txt".into(), "crlf.txt".into(), "locked.bin".into()],
            &ctx(),
        )
        .await
        .expect("add");
    for (path, name, value) in [
        ("kw.txt", "svn:keywords", "Id Rev Author"),
        ("crlf.txt", "svn:eol-style", "CRLF"),
        ("locked.bin", "svn:needs-lock", "*"),
    ] {
        e.engine
            .props_set(&wc, path, name, Some(value), &ctx())
            .await
            .unwrap_or_else(|err| panic!("propset {name}: {err:?}"));
    }
    let rev = commit(&e, &wc, "props").await.expect("commit");
    let fresh = checkout(&e, &url, "props-fresh").await;
    let kw = std::fs::read_to_string(fresh.root.join("kw.txt")).unwrap();
    assert!(kw.contains(&format!("$Rev: {} $", rev.0)), "{kw}");
    assert!(kw.contains("$Author: alice $"), "{kw}");
    assert!(kw.starts_with("$Id: kw.txt "), "{kw}");
    assert_eq!(
        std::fs::read(fresh.root.join("crlf.txt")).unwrap(),
        b"a\r\nb\r\n"
    );
    // The committing working copy is translated too.
    let kw_own = std::fs::read_to_string(wc.root.join("kw.txt")).unwrap();
    assert!(kw_own.contains(&format!("$Rev: {} $", rev.0)), "{kw_own}");
    let ro = |root: &Path| {
        std::fs::metadata(root.join("locked.bin"))
            .unwrap()
            .permissions()
            .readonly()
    };
    assert!(
        ro(&fresh.root),
        "needs-lock file is read-only after checkout"
    );
    let scope = LockScope::Wc {
        wc: fresh.clone(),
        paths: vec!["locked.bin".into()],
    };
    e.engine
        .lock(&scope, Some("Khóa để sửa"), false, &ctx())
        .await
        .expect("lock");
    assert!(!ro(&fresh.root), "writable while locked");
    e.engine
        .unlock(&scope, false, &ctx())
        .await
        .expect("unlock");
    assert!(ro(&fresh.root), "read-only again after unlock");
    // Clean status everywhere.
    let st = e
        .engine
        .status(
            &fresh,
            StatusQuery {
                include_ignored: false,
                include_unversioned: true,
            },
            &ctx(),
        )
        .await
        .expect("status");
    assert!(
        st.iter().all(|s| s.text == opensvn_ipc::NodeStatus::Normal),
        "{st:?}"
    );
}

fn peak_rss_mb() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    s.lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .map_or(0, |kb| kb / 1024)
}

/// A 100 MB+ binary file round trip (commit, fresh checkout, update after a change).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the VisualSVN-like server (scripts/test-env/visualsvn-like.sh); slow"]
async fn big_binary_file_round_trip() {
    use sha1::Digest as _;
    let mb: usize = var("OPENSVN_VSVN_BIG_MB", "120").parse().unwrap();
    let repo = Repo::new("big");
    let e = sso();
    let url = repo.url();
    let wc = checkout(&e, &url, "big").await;
    // Pseudo-random (incompressible) content.
    let mut data = vec![0u8; mb << 20];
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for chunk in data.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        chunk.copy_from_slice(&x.to_le_bytes()[..chunk.len()]);
    }
    let digest = sha1::Sha1::digest(&data);
    write(&wc.root, "big.bin", &data);
    drop(data);
    let rss0 = peak_rss_mb();
    let t = Instant::now();
    commit(&e, &wc, "big").await.expect("commit big file");
    eprintln!(
        "commit {mb} MB: {:?}, peak RSS {} MB (before {rss0} MB)",
        t.elapsed(),
        peak_rss_mb()
    );
    let t = Instant::now();
    let fresh = checkout(&e, &url, "big-fresh").await;
    eprintln!(
        "checkout {mb} MB: {:?}, peak RSS {} MB",
        t.elapsed(),
        peak_rss_mb()
    );
    let back = std::fs::read(fresh.root.join("big.bin")).unwrap();
    assert_eq!(sha1::Sha1::digest(&back), digest);
    // Change one byte in the middle: update sends a delta.
    let mut changed = back;
    let mid = changed.len() / 2;
    changed[mid] ^= 0xff;
    write(&fresh.root, "big.bin", &changed);
    let digest2 = sha1::Sha1::digest(&changed);
    drop(changed);
    let t = Instant::now();
    commit(&e, &fresh, "one byte").await.expect("commit change");
    eprintln!("commit 1-byte change: {:?}", t.elapsed());
    let t = Instant::now();
    update(&e, &wc).await;
    eprintln!(
        "update 1-byte change: {:?}, peak RSS {} MB",
        t.elapsed(),
        peak_rss_mb()
    );
    let back = std::fs::read(wc.root.join("big.bin")).unwrap();
    assert_eq!(sha1::Sha1::digest(&back), digest2);
}

/// 10k files in one commit, a fresh checkout, status and an update.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the VisualSVN-like server (scripts/test-env/visualsvn-like.sh); slow"]
async fn big_many_files() {
    let n: usize = var("OPENSVN_VSVN_MANY_FILES", "10000").parse().unwrap();
    let repo = Repo::new("many");
    let e = sso();
    let url = repo.url();
    let wc = checkout(&e, &url, "many").await;
    for i in 0..n {
        write(
            &wc.root,
            &format!("d{:03}/f{i:05}.txt", i / 100),
            format!("file {i}\n"),
        );
    }
    let t = Instant::now();
    commit(&e, &wc, "many files").await.expect("commit");
    eprintln!("commit {n} files: {:?}", t.elapsed());
    let t = Instant::now();
    let fresh = checkout(&e, &url, "many-fresh").await;
    eprintln!("checkout {n} files: {:?}", t.elapsed());
    let t = Instant::now();
    let st = e
        .engine
        .status(
            &fresh,
            StatusQuery {
                include_ignored: false,
                include_unversioned: true,
            },
            &ctx(),
        )
        .await
        .expect("status");
    eprintln!("status {n} files: {:?}", t.elapsed());
    assert!(st.is_empty(), "{} entries", st.len());
    write(&wc.root, "d000/f00000.txt", "changed\n");
    commit(&e, &wc, "one change").await.expect("commit");
    let t = Instant::now();
    update(&e, &fresh).await;
    eprintln!("update with {n} files: {:?}", t.elapsed());
    assert_eq!(
        std::fs::read_to_string(fresh.root.join("d000/f00000.txt")).unwrap(),
        "changed\n"
    );
    let t = Instant::now();
    let scan = e
        .engine
        .status_remote(&fresh, &ctx())
        .await
        .expect("status_remote");
    eprintln!("status_remote with {n} files: {:?}", t.elapsed());
    assert_eq!(scan.incoming, 0);
}
