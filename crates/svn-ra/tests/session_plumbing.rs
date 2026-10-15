//! Session plumbing for the engine: reparenting without a request, unattended sessions that
//! never prompt, and the repository-root ancestor check at open (issue #78).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;
mod ra_write_support;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::{Reply, ScriptedPrompter, TestCa, TlsServer, basic, transport_trusting};
use ra_write_support::{Bulk, FixedPrompter, ROOT, fixture, server, session_url, transport};
use svn_ra::ops::HttpRaSession;
use svn_ra_api::RaSession;
use svn_types::{CancelToken, ErrorCode, NodeKind, RelPath, RepoUrl, Revnum};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn p(s: &str) -> RelPath {
    RelPath::new(s).unwrap()
}

async fn open(srv: &MockServer) -> HttpRaSession {
    HttpRaSession::open(
        transport(Arc::new(FixedPrompter::default())),
        session_url(srv),
        &CancelToken::new(),
    )
    .await
    .expect("open session")
}

async fn request_count(srv: &MockServer) -> usize {
    srv.received_requests().await.unwrap_or_default().len()
}

#[tokio::test]
async fn reparent_moves_the_session_without_a_request() {
    let srv = server(Bulk::Prefer).await;
    Mock::given(method("PROPFIND"))
        .and(path("/svn/repo/!svn/rvr/5/trunk/a.txt"))
        .respond_with(
            ResponseTemplate::new(207)
                .insert_header("Content-Type", "text/xml; charset=\"utf-8\"")
                .set_body_bytes(fixture("propfind/stat-file.resp")),
        )
        .mount(&srv)
        .await;
    let s = open(&srv).await;
    let before = request_count(&srv).await;
    let root_url = RepoUrl::parse(&format!("{}{ROOT}", srv.uri())).unwrap();
    let r = s.reparent(&root_url).expect("reparent to the root");
    assert_eq!(request_count(&srv).await, before, "reparent sends nothing");
    assert_eq!(r.session_url(), &root_url);
    assert_eq!(r.repos_root(), s.repos_root());
    assert_eq!(r.uuid(), s.uuid());
    assert_eq!(r.wire().session_rel().as_str(), "");
    assert!(Arc::ptr_eq(r.wire().transport(), s.wire().transport()));
    // Paths are now relative to the root: trunk/a.txt is the same node as a.txt before.
    let cancel = CancelToken::new();
    assert_eq!(
        r.check_path(&p("trunk/a.txt"), Some(Revnum(5)), &cancel)
            .await
            .unwrap(),
        NodeKind::File
    );
    assert_eq!(
        s.check_path(&p("a.txt"), Some(Revnum(5)), &cancel)
            .await
            .unwrap(),
        NodeKind::File
    );
    // And back down again.
    let deeper = r
        .reparent(&RepoUrl::parse(&format!("{}{ROOT}/trunk/src", srv.uri())).unwrap())
        .unwrap();
    assert_eq!(deeper.wire().session_rel().as_str(), "trunk/src");
}

#[tokio::test]
async fn reparent_outside_the_repository_is_refused() {
    let srv = server(Bulk::Prefer).await;
    let s = open(&srv).await;
    let before = request_count(&srv).await;
    let port = srv.address().port();
    for url in [
        format!("{}/svn/other", srv.uri()),
        format!("{}/svn", srv.uri()),
        format!("{}/svn/repository/trunk", srv.uri()),
        format!("{}{ROOT}/!svn/me", srv.uri()),
        format!("{}{ROOT}/!svn/rvr/5/trunk", srv.uri()),
        format!("http://localhost:{port}{ROOT}/trunk"),
        format!("https://127.0.0.1:{port}{ROOT}/trunk"),
        format!("http://127.0.0.1:{}{ROOT}/trunk", port.wrapping_add(1)),
    ] {
        let err = s.reparent(&RepoUrl::parse(&url).unwrap()).expect_err(&url);
        assert_eq!(err.code, ErrorCode::UnsafePath, "{url}: {err:?}");
    }
    assert_eq!(request_count(&srv).await, before);
}

#[tokio::test]
async fn reparent_accepts_a_root_spelled_in_another_case() {
    let srv = server(Bulk::Prefer).await;
    let s = open(&srv).await;
    let r = s
        .reparent(&RepoUrl::parse(&format!("{}/SVN/Repo/Trunk", srv.uri())).unwrap())
        .expect("case-insensitive root (VisualSVN)");
    // The root part takes the server's spelling; the rest keeps the caller's.
    assert_eq!(
        r.session_url().as_url().as_str(),
        format!("{}{ROOT}/Trunk", srv.uri())
    );
    assert_eq!(r.wire().session_rel().as_str(), "Trunk");
}

/// An OPTIONS answer whose repository root is spelled differently from the session URL (as
/// VisualSVN does on Windows) still opens; the session URL takes the server's spelling.
#[tokio::test]
async fn open_accepts_a_differently_cased_root_and_rejects_a_foreign_one() {
    async fn options_server(root: &str) -> MockServer {
        let srv = MockServer::start().await;
        Mock::given(method("OPTIONS"))
            .respond_with(
                ResponseTemplate::new(200)
                    .append_header("DAV", "1,2")
                    .append_header("SVN-Youngest-Rev", "5")
                    .append_header(
                        "SVN-Repository-UUID",
                        "7462d98e-4071-4508-b797-d2922b5cda8f",
                    )
                    .append_header("SVN-Repository-Root", root)
                    .append_header("SVN-Me-Resource", format!("{root}/!svn/me"))
                    .append_header("SVN-Rev-Root-Stub", format!("{root}/!svn/rvr"))
                    .append_header("SVN-Rev-Stub", format!("{root}/!svn/rev")),
            )
            .mount(&srv)
            .await;
        srv
    }
    let srv = options_server("/svn/MyRepo").await;
    let s = HttpRaSession::open(
        transport(Arc::new(FixedPrompter::default())),
        RepoUrl::parse(&format!("{}/svn/myrepo/trunk", srv.uri())).unwrap(),
        &CancelToken::new(),
    )
    .await
    .expect("case-insensitive match");
    assert_eq!(
        s.session_url().as_url().as_str(),
        format!("{}/svn/MyRepo/trunk", srv.uri())
    );
    assert_eq!(
        s.repos_root().as_url().as_str(),
        format!("{}/svn/MyRepo", srv.uri())
    );

    let srv = options_server("/elsewhere").await;
    let err = HttpRaSession::open(
        transport(Arc::new(FixedPrompter::default())),
        RepoUrl::parse(&format!("{}/svn/repo/trunk", srv.uri())).unwrap(),
        &CancelToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::UnsafePath);
}

/// HTTPS front that wants Basic `alice`/`pw` for everything and answers OPTIONS like
/// mod_dav_svn once authenticated.
async fn auth_server() -> (TestCa, TlsServer) {
    let ca = TestCa::new();
    let handler: Arc<common::Handler> = Arc::new(|seen: &common::Seen| {
        if seen.header("authorization").as_deref() != Some(basic("alice", "pw").as_str()) {
            return Reply::unauthorized("r");
        }
        Reply::ok("")
            .header("DAV", "1,2")
            .header("SVN-Youngest-Rev", "7")
            .header(
                "SVN-Repository-UUID",
                "7462d98e-4071-4508-b797-d2922b5cda8f",
            )
            .header("SVN-Repository-Root", ROOT)
            .header("SVN-Me-Resource", "/svn/repo/!svn/me")
            .header("SVN-Rev-Root-Stub", "/svn/repo/!svn/rvr")
            .header("SVN-Rev-Stub", "/svn/repo/!svn/rev")
    });
    let srv = TlsServer::start(&ca.leaf(&["localhost"], false), handler).await;
    (ca, srv)
}

fn tls_session_url(srv: &TlsServer) -> RepoUrl {
    RepoUrl::parse(srv.url("/svn/repo/trunk").as_str()).unwrap()
}

#[tokio::test]
async fn unattended_session_fails_with_auth_required_and_never_prompts() {
    let (ca, srv) = auth_server().await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    let err =
        HttpRaSession::open_unattended(Arc::clone(&t), tls_session_url(&srv), &CancelToken::new())
            .await
            .expect_err("needs credentials");
    assert_eq!(err.code, ErrorCode::AuthRequired);
    assert_eq!(prompter.credential_prompts(), 0);

    // An interactive session prompts once; its unattended twin reuses what was cached.
    let s = HttpRaSession::open(Arc::clone(&t), tls_session_url(&srv), &CancelToken::new())
        .await
        .expect("interactive open");
    assert_eq!(prompter.credential_prompts(), 1);
    let u = s.unattended();
    assert!(u.wire().is_unattended());
    assert!(!s.wire().is_unattended());
    assert_eq!(
        u.youngest_rev(&CancelToken::new()).await.unwrap(),
        Revnum(7)
    );
    assert!(u.reparent(s.repos_root()).unwrap().wire().is_unattended());
    assert_eq!(prompter.credential_prompts(), 1);
}

#[tokio::test]
async fn unattended_session_on_a_fresh_transport_never_prompts() {
    let (ca, srv) = auth_server().await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    // Open interactively on another transport so the unattended session exists, then talk to
    // the server through a transport that has nothing cached.
    let other = transport_trusting(
        &ca,
        ScriptedPrompter::with_credentials("alice", "pw"),
        |_| {},
    );
    let s = HttpRaSession::open(other, tls_session_url(&srv), &CancelToken::new())
        .await
        .unwrap();
    let fresh = HttpRaSession::from_wire(
        svn_ra::ops::update::RaWire::new(t, s.session_url().clone(), s.wire().caps().clone())
            .unwrap()
            .unattended(),
    )
    .unwrap();
    let err = fresh
        .youngest_rev(&CancelToken::new())
        .await
        .expect_err("401 without credentials");
    assert_eq!(err.code, ErrorCode::AuthRequired);
    assert_eq!(prompter.credential_prompts(), 0);
}

#[tokio::test]
async fn unattended_session_uses_stored_credentials_silently() {
    let (ca, srv) = auth_server().await;
    let prompter = ScriptedPrompter::with_stored("alice", "pw");
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    let s = HttpRaSession::open_unattended(t, tls_session_url(&srv), &CancelToken::new())
        .await
        .expect("stored credentials");
    assert_eq!(
        s.youngest_rev(&CancelToken::new()).await.unwrap(),
        Revnum(7)
    );
    assert_eq!(prompter.credential_prompts(), 0);
    assert_eq!(prompter.stored_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_session_reports_the_basic_user_it_authenticated_as() {
    let (ca, srv) = auth_server().await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    let s = HttpRaSession::open(Arc::clone(&t), tls_session_url(&srv), &CancelToken::new())
        .await
        .expect("interactive open");
    assert_eq!(s.authenticated_user().as_deref(), Some("alice"));
    // The root session of the same transport and its unattended twin know it too.
    let root = s.reparent(s.repos_root()).unwrap();
    assert_eq!(root.authenticated_user().as_deref(), Some("alice"));
    assert_eq!(
        s.unattended().authenticated_user().as_deref(),
        Some("alice")
    );
    // A transport that never authenticated knows nothing.
    let other = transport_trusting(&ca, ScriptedPrompter::with_credentials("bob", "pw"), |_| {});
    let fresh = HttpRaSession::from_wire(
        svn_ra::ops::update::RaWire::new(other, s.session_url().clone(), s.wire().caps().clone())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(fresh.authenticated_user(), None);
}

/// Path-based authorization that forbids the repository root and allows `/trunk`: a session
/// opened at `trunk` and reparented to the root still learns the youngest revision (asked where
/// the session was opened) and reads the log of `trunk` (the REPORT goes to `trunk`, not to the
/// root).
#[tokio::test]
async fn reparented_root_session_works_when_the_root_is_forbidden() {
    let srv = server(Bulk::Prefer).await;
    Mock::given(method("OPTIONS"))
        .and(path(ROOT))
        .respond_with(ResponseTemplate::new(403))
        .with_priority(1)
        .mount(&srv)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/svn/repo/!svn/rvr/5/trunk"))
        .and(wiremock::matchers::body_string_contains(
            "<S:path></S:path>",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/xml; charset=\"utf-8\"")
                .set_body_bytes(fixture("log/head-verbose.resp")),
        )
        .mount(&srv)
        .await;
    Mock::given(method("REPORT"))
        .respond_with(ResponseTemplate::new(403))
        .with_priority(10)
        .mount(&srv)
        .await;
    let s = open(&srv).await;
    let root = s.reparent(s.repos_root()).unwrap();
    let cancel = CancelToken::new();
    assert_eq!(root.youngest_rev(&cancel).await.unwrap(), Revnum(5));

    let mut seen = Vec::new();
    root.log(
        &svn_ra_api::LogRequest {
            paths: vec![p("trunk")],
            start: None,
            end: None,
            limit: None,
            discover_changed_paths: true,
            strict_node_history: false,
        },
        &mut |e| {
            seen.push(e);
            Ok(())
        },
        &cancel,
    )
    .await
    .expect("log of trunk through the root session");
    assert!(!seen.is_empty());

    // A session opened at the root itself has nowhere else to ask: the 403 stands.
    let at_root = HttpRaSession::open(
        transport(Arc::new(FixedPrompter::default())),
        RepoUrl::parse(&format!("{}{ROOT}", srv.uri())).unwrap(),
        &cancel,
    )
    .await
    .expect_err("root is forbidden");
    assert_eq!(at_root.code, ErrorCode::Forbidden);
}

/// mod_dav_svn's answer to `OPTIONS` on an `SVNParentPath` location: a bare 500.
fn parent_path_options_500() -> ResponseTemplate {
    ResponseTemplate::new(500)
        .insert_header("Content-Type", "text/html; charset=iso-8859-1")
        .set_body_string("<html><title>500 Internal Server Error</title></html>")
}

#[tokio::test]
async fn opening_the_repository_list_of_a_parent_path_names_the_repositories() {
    let srv = MockServer::start().await;
    Mock::given(method("OPTIONS"))
        .and(path("/svn"))
        .respond_with(parent_path_options_500())
        .mount(&srv)
        .await;
    // VisualSVN serves the XML index with an XSLT stylesheet.
    Mock::given(method("GET"))
        .and(path("/svn/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<?xml version="1.0"?>
<?xml-stylesheet type="text/xsl" href="/svnindex.xsl"?>
<svn version="1.14.3 (r1914484)" href="http://subversion.apache.org/">
  <index rev="0" path="Collection of Repositories">
    <dir name="Dự án" href="D%e1%bb%b1%20%c3%a1n/" />
    <dir name="MyRepo" href="MyRepo/" />
  </index>
</svn>"#,
        ))
        .mount(&srv)
        .await;
    let url = RepoUrl::parse(&format!("{}/svn", srv.uri())).unwrap();
    let Err(err) = HttpRaSession::open(
        transport(Arc::new(FixedPrompter::default())),
        url,
        &CancelToken::new(),
    )
    .await
    else {
        panic!("a repository list is not a repository");
    };
    assert_eq!(err.code, ErrorCode::PathNotFound, "{err:?}");
    assert!(
        err.message.contains("list of repositories")
            && err.message.contains("Repositories here: Dự án, MyRepo"),
        "{}",
        err.message
    );
    let urls: Vec<(&str, String)> = err
        .parent_repositories()
        .expect("the repositories ride on the error")
        .iter()
        .map(|r| (r.name.as_str(), r.url.clone()))
        .collect();
    assert_eq!(
        urls,
        [
            ("Dự án", format!("{}/svn/D%E1%BB%B1%20%C3%A1n", srv.uri())),
            ("MyRepo", format!("{}/svn/MyRepo", srv.uri())),
        ]
    );
}

/// Apache's `SVNParentPath` HTML listing (`SVNListParentPath On`, no `SVNIndexXSLT`).
const PARENT_HTML: &str = r#"<html><head><title>Collection of Repositories</title></head>
<body>
 <h2>Collection of Repositories</h2>
 <ul>
  <li><a href="D%c6%b0%cc%a3%20a%cc%81n%20m%e1%bb%9bi/">Dự án mới/</a></li>
  <li><a href="My%20Project/">My Project/</a></li>
 </ul>
 <hr noshade><em>Powered by <a href="http://subversion.apache.org/">Apache Subversion</a></em>
</body></html>"#;

#[tokio::test]
async fn the_repository_list_behind_basic_auth_is_read_with_the_same_sign_in() {
    let ca = TestCa::new();
    let handler: Arc<common::Handler> = Arc::new(|seen: &common::Seen| {
        if seen.header("authorization").as_deref() != Some(basic("alice", "pw").as_str()) {
            return Reply::unauthorized("VisualSVN Server");
        }
        match (seen.method.as_str(), seen.path.as_str()) {
            ("GET", "/svn/") => Reply::ok(PARENT_HTML),
            _ => Reply::status(500).header("content-type", "text/html; charset=iso-8859-1"),
        }
    });
    let srv = TlsServer::start(&ca.leaf(&["localhost"], false), handler).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let t = transport_trusting(&ca, prompter.clone(), |_| {});
    let url = RepoUrl::parse(srv.url("/svn").as_str()).unwrap();
    let Err(err) = HttpRaSession::open(t, url, &CancelToken::new()).await else {
        panic!("a repository list is not a repository");
    };
    assert_eq!(err.code, ErrorCode::PathNotFound, "{err:?}");
    let repos = err.parent_repositories().expect("repositories");
    let base = srv.url("/svn").as_str().trim_end_matches('/').to_owned();
    let got: Vec<(&str, String)> = repos
        .iter()
        .map(|r| (r.name.as_str(), r.url.clone()))
        .collect();
    assert_eq!(
        got,
        [
            // Decomposed Vietnamese (as macOS writes it) stays as the server spelled it.
            (
                "Dư\u{323} a\u{301}n mới",
                format!("{base}/D%C6%B0%CC%A3%20a%CC%81n%20m%E1%BB%9Bi")
            ),
            ("My Project", format!("{base}/My%20Project")),
        ]
    );
    assert_eq!(
        prompter.credential_prompts(),
        1,
        "one sign-in serves the OPTIONS and the listing"
    );
}

#[tokio::test]
async fn a_refused_sign_in_on_a_repository_list_stays_auth_required() {
    let srv = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(
            ResponseTemplate::new(401).insert_header("WWW-Authenticate", "Basic realm=\"SVN\""),
        )
        .mount(&srv)
        .await;
    let url = RepoUrl::parse(&format!("{}/svn", srv.uri())).unwrap();
    let Err(err) = HttpRaSession::open(
        transport(Arc::new(ScriptedPrompter::default())),
        url,
        &CancelToken::new(),
    )
    .await
    else {
        panic!("no credentials");
    };
    assert_eq!(err.code, ErrorCode::AuthRequired, "{err:?}");
    assert!(err.parent_repositories().is_none());
}

#[tokio::test]
async fn a_listing_that_needs_a_refused_sign_in_reports_the_sign_in() {
    // OPTIONS answers the bare 500 without asking; only the listing asks for a sign-in.
    let srv = MockServer::start().await;
    Mock::given(method("OPTIONS"))
        .respond_with(parent_path_options_500())
        .mount(&srv)
        .await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(401).insert_header("WWW-Authenticate", "Basic realm=\"SVN\""),
        )
        .mount(&srv)
        .await;
    let url = RepoUrl::parse(&format!("{}/svn", srv.uri())).unwrap();
    let Err(err) = HttpRaSession::open(
        transport(Arc::new(ScriptedPrompter::default())),
        url,
        &CancelToken::new(),
    )
    .await
    else {
        panic!("no credentials");
    };
    assert_eq!(err.code, ErrorCode::AuthRequired, "{err:?}");
    assert!(err.parent_repositories().is_none());
}

#[tokio::test]
async fn cancelling_while_the_listing_is_read_reports_the_cancel() {
    let srv = MockServer::start().await;
    Mock::given(method("OPTIONS"))
        .respond_with(parent_path_options_500())
        .mount(&srv)
        .await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(PARENT_HTML)
                .set_delay(std::time::Duration::from_secs(30)),
        )
        .mount(&srv)
        .await;
    let url = RepoUrl::parse(&format!("{}/svn", srv.uri())).unwrap();
    let cancel = CancelToken::new();
    let later = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        later.cancel();
    });
    let Err(err) =
        HttpRaSession::open(transport(Arc::new(FixedPrompter::default())), url, &cancel).await
    else {
        panic!("cancelled");
    };
    assert_eq!(err.code, ErrorCode::Cancelled, "{err:?}");
    assert!(err.parent_repositories().is_none());
}

#[tokio::test]
async fn a_plain_server_error_on_open_stays_a_server_error() {
    let srv = MockServer::start().await;
    Mock::given(method("OPTIONS"))
        .respond_with(parent_path_options_500())
        .mount(&srv)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>It works!</html>"))
        .mount(&srv)
        .await;
    let url = RepoUrl::parse(&format!("{}/svn/repo", srv.uri())).unwrap();
    let Err(err) = HttpRaSession::open(
        transport(Arc::new(FixedPrompter::default())),
        url,
        &CancelToken::new(),
    )
    .await
    else {
        panic!("server error");
    };
    assert_ne!(err.code, ErrorCode::PathNotFound, "{err:?}");
    assert!(err.message.contains("HTTP 500"), "{err:?}");
}
