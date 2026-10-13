//! Read operations of `HttpRaSession` against a scripted mod_dav_svn front, using response
//! bodies captured from svn 1.15 (`svn-protocol/tests/data`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod ra_write_support;

use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use md5::Digest as _;

use ra_write_support::{
    Bulk, FixedPrompter, ROOT, fixture, requests, server, session_url, transport, wire,
};
use svn_delta::WindowApplier;
use svn_ra::ops::HttpRaSession;
use svn_ra::ops::read::ListLimits;
use svn_ra_api::{FileRev, FileRevHandler, LogRequest, RaSession};
use svn_types::{
    CancelToken, Checksum, Depth, ErrorCode, LogEntry, NodeKind, RelPath, Result, Revnum,
    TxDeltaWindow, WindowSink,
};
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn p(s: &str) -> RelPath {
    RelPath::new(s).unwrap()
}

async fn session(srv: &MockServer) -> HttpRaSession {
    HttpRaSession::open(
        transport(Arc::new(FixedPrompter::default())),
        session_url(srv),
        &CancelToken::new(),
    )
    .await
    .expect("open session")
}

fn xml(status: u16, body: impl Into<Vec<u8>>) -> ResponseTemplate {
    ResponseTemplate::new(status)
        .insert_header("Content-Type", "text/xml; charset=\"utf-8\"")
        .set_body_bytes(body.into())
}

async fn mount(srv: &MockServer, m: &str, p: &str, resp: ResponseTemplate) {
    Mock::given(method(m))
        .and(path(p))
        .respond_with(resp)
        .mount(srv)
        .await;
}

/// One `D:response` of a Depth-1 listing.
fn response(href: &str, dir: bool, rev: u64) -> String {
    let rt = if dir { "<D:collection/>" } else { "" };
    let len = if dir {
        String::new()
    } else {
        "<D:getcontentlength>4</D:getcontentlength>".to_owned()
    };
    format!(
        "<D:response><D:href>{href}</D:href><D:propstat><D:prop>\
         <D:resourcetype>{rt}</D:resourcetype>{len}\
         <D:version-name>{rev}</D:version-name>\
         <D:creator-displayname>alice</D:creator-displayname>\
         <D:creationdate>2026-10-13T09:45:07.190780Z</D:creationdate>\
         </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
    )
}

fn multistatus(responses: &[String]) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:multistatus xmlns:D=\"DAV:\">{}</D:multistatus>",
        responses.concat()
    )
}

#[tokio::test]
async fn open_reports_root_uuid_and_youngest() {
    let srv = server(Bulk::Prefer).await;
    let s = session(&srv).await;
    assert_eq!(
        s.repos_root().as_url().as_str(),
        format!("{}{ROOT}", srv.uri())
    );
    assert_eq!(s.uuid(), "7462d98e-4071-4508-b797-d2922b5cda8f");
    assert_eq!(s.session_url(), &session_url(&srv));
    let cancel = CancelToken::new();
    assert_eq!(s.youngest_rev(&cancel).await.unwrap(), Revnum(5));
    s.ensure_authenticated(&cancel).await.unwrap();
}

#[tokio::test]
async fn open_refuses_a_server_without_httpv2() {
    let srv = MockServer::start().await;
    Mock::given(method("OPTIONS"))
        .respond_with(ResponseTemplate::new(200).append_header("DAV", "1,2"))
        .mount(&srv)
        .await;
    let err = HttpRaSession::open(
        transport(Arc::new(FixedPrompter::default())),
        session_url(&srv),
        &CancelToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::ServerUnsupported);
}

#[tokio::test]
async fn stat_and_check_path() {
    let srv = server(Bulk::Prefer).await;
    mount(
        &srv,
        "PROPFIND",
        "/svn/repo/!svn/rvr/5/trunk/a.txt",
        xml(207, fixture("propfind/stat-file.resp")),
    )
    .await;
    mount(
        &srv,
        "PROPFIND",
        "/svn/repo/!svn/rvr/5/trunk/nope.txt",
        ResponseTemplate::new(404).set_body_bytes(fixture("propfind/stat-missing-404.resp")),
    )
    .await;
    let s = session(&srv).await;
    let cancel = CancelToken::new();
    let e = s.stat(&p("a.txt"), None, &cancel).await.unwrap().unwrap();
    assert_eq!(e.name, "a.txt");
    assert_eq!(e.kind, NodeKind::File);
    assert_eq!(e.size, Some(17));
    assert_eq!(e.created_rev, Some(Revnum(3)));
    assert_eq!(e.last_author.as_deref(), Some("alice"));
    assert!(e.has_props);
    assert_eq!(
        s.check_path(&p("a.txt"), Some(Revnum(5)), &cancel)
            .await
            .unwrap(),
        NodeKind::File
    );
    assert!(
        s.stat(&p("nope.txt"), None, &cancel)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        s.check_path(&p("nope.txt"), Some(Revnum(5)), &cancel)
            .await
            .unwrap(),
        NodeKind::None
    );
}

#[tokio::test]
async fn a_missing_revision_is_an_error_not_an_absent_node() {
    let srv = server(Bulk::Prefer).await;
    let body = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:error xmlns:D=\"DAV:\" \
                xmlns:m=\"http://apache.org/dav/xmlns\" xmlns:C=\"svn:\"><C:error/>\
                <m:human-readable errcode=\"160006\">No such revision 99</m:human-readable></D:error>";
    mount(
        &srv,
        "PROPFIND",
        "/svn/repo/!svn/rvr/99/trunk/a.txt",
        xml(404, body),
    )
    .await;
    let s = session(&srv).await;
    let err = s
        .check_path(&p("a.txt"), Some(Revnum(99)), &CancelToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.svn_code, Some(160_006));
}

#[tokio::test]
async fn list_depths() {
    let srv = server(Bulk::Prefer).await;
    mount(
        &srv,
        "PROPFIND",
        "/svn/repo/!svn/rvr/5/trunk",
        xml(207, fixture("propfind/list-depth1.resp")),
    )
    .await;
    let s = session(&srv).await;
    let cancel = CancelToken::new();
    let all = s
        .list(&RelPath::root(), None, Depth::Immediates, &cancel)
        .await
        .unwrap();
    // Server order, percent-decoded names.
    let names: Vec<_> = all.iter().map(|e| (e.name.as_str(), e.kind)).collect();
    assert_eq!(
        names,
        [
            ("src", NodeKind::Dir),
            ("docs", NodeKind::Dir),
            ("unicodé file.txt", NodeKind::File),
            ("a.txt", NodeKind::File),
        ]
    );
    let files = s
        .list(&RelPath::root(), None, Depth::Files, &cancel)
        .await
        .unwrap();
    assert!(files.iter().all(|e| e.kind == NodeKind::File));
    assert_eq!(files.len(), 2);
    assert!(
        s.list(&RelPath::root(), None, Depth::Empty, &cancel)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn list_infinity_walks_the_tree() {
    let srv = server(Bulk::Prefer).await;
    let rvr = "/svn/repo/!svn/rvr/7";
    mount(
        &srv,
        "PROPFIND",
        &format!("{rvr}/trunk"),
        xml(
            207,
            multistatus(&[
                response(&format!("{rvr}/trunk/"), true, 7),
                response(&format!("{rvr}/trunk/a.txt"), false, 3),
                response(&format!("{rvr}/trunk/src/"), true, 7),
            ]),
        ),
    )
    .await;
    mount(
        &srv,
        "PROPFIND",
        &format!("{rvr}/trunk/src"),
        xml(
            207,
            multistatus(&[
                response(&format!("{rvr}/trunk/src/"), true, 7),
                response(&format!("{rvr}/trunk/src/deep/"), true, 6),
                response(&format!("{rvr}/trunk/src/b.txt"), false, 4),
            ]),
        ),
    )
    .await;
    mount(
        &srv,
        "PROPFIND",
        &format!("{rvr}/trunk/src/deep"),
        xml(
            207,
            multistatus(&[
                response(&format!("{rvr}/trunk/src/deep/"), true, 6),
                response(&format!("{rvr}/trunk/src/deep/c.cfg"), false, 6),
            ]),
        ),
    )
    .await;
    let s = session(&srv).await;
    let entries = s
        .list(
            &RelPath::root(),
            Some(Revnum(7)),
            Depth::Infinity,
            &CancelToken::new(),
        )
        .await
        .unwrap();
    let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        ["a.txt", "src", "src/deep", "src/deep/c.cfg", "src/b.txt"]
    );
}

#[tokio::test]
async fn list_of_a_file_is_refused() {
    let srv = server(Bulk::Prefer).await;
    mount(
        &srv,
        "PROPFIND",
        "/svn/repo/!svn/rvr/5/trunk/a.txt",
        xml(207, fixture("propfind/stat-file.resp")),
    )
    .await;
    let s = session(&srv).await;
    let err = s
        .list(
            &p("a.txt"),
            Some(Revnum(5)),
            Depth::Immediates,
            &CancelToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PathNotFound);
}

#[tokio::test]
async fn hostile_hrefs_are_refused_or_skipped() {
    let srv = server(Bulk::Prefer).await;
    let rvr = "/svn/repo/!svn/rvr/5";
    mount(
        &srv,
        "PROPFIND",
        &format!("{rvr}/trunk/x"),
        xml(
            207,
            multistatus(&[response(
                "http://evil.test.local/svn/repo/!svn/rvr/5/trunk/x",
                false,
                1,
            )]),
        ),
    )
    .await;
    mount(
        &srv,
        "PROPFIND",
        &format!("{rvr}/trunk"),
        xml(
            207,
            multistatus(&[
                response(&format!("{rvr}/trunk"), true, 5),
                response(&format!("{rvr}/trunk/ok.txt"), false, 5),
                response(
                    "http://evil.test.local/svn/repo/!svn/rvr/5/trunk/evil",
                    false,
                    5,
                ),
                response(&format!("{rvr}/trunk/%2e%2e/escape"), false, 5),
            ]),
        ),
    )
    .await;
    let s = session(&srv).await;
    let cancel = CancelToken::new();
    let err = s.stat(&p("x"), None, &cancel).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::UnsafePath);
    let names: Vec<_> = s
        .list(&RelPath::root(), None, Depth::Immediates, &cancel)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["ok.txt"]);
    let reqs = requests(&srv).await;
    assert!(reqs.iter().all(|(_, p)| p.starts_with("/svn/repo/")));
}

async fn mount_file(srv: &MockServer, content: &[u8]) {
    mount(
        srv,
        "PROPFIND",
        "/svn/repo/!svn/rvr/5/trunk/a.txt",
        xml(207, fixture("propfind/allprop-file.resp")),
    )
    .await;
    mount(
        srv,
        "GET",
        "/svn/repo/!svn/rvr/5/trunk/a.txt",
        ResponseTemplate::new(200).set_body_bytes(content.to_vec()),
    )
    .await;
}

#[tokio::test]
async fn get_file_streams_content_and_returns_props() {
    let srv = server(Bulk::Prefer).await;
    let content = fixture("get/file.resp");
    mount_file(&srv, &content).await;
    let s = session(&srv).await;
    let mut out = Vec::new();
    let meta = s
        .get_file(&p("a.txt"), None, &mut out, &CancelToken::new())
        .await
        .unwrap();
    assert_eq!(out, content);
    assert_eq!(meta.rev, Revnum(5));
    let tag = meta
        .props
        .iter()
        .find(|c| c.name == "custom:tag")
        .expect("custom property");
    assert_eq!(tag.value.as_deref(), Some(&b"v<1>&\"q\""[..]));
    assert!(meta.props.iter().all(|c| !c.name.starts_with("svn:entry")));
}

#[tokio::test]
async fn get_file_detects_a_checksum_mismatch() {
    let srv = server(Bulk::Prefer).await;
    mount_file(&srv, b"tampered\n").await;
    let s = session(&srv).await;
    let mut out = Vec::new();
    let err = s
        .get_file(&p("a.txt"), None, &mut out, &CancelToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Corrupt);
}

#[tokio::test]
async fn get_file_of_a_directory_sends_no_get() {
    let srv = server(Bulk::Prefer).await;
    let rvr = "/svn/repo/!svn/rvr/5";
    mount(
        &srv,
        "PROPFIND",
        &format!("{rvr}/trunk/src"),
        xml(
            207,
            multistatus(&[response(&format!("{rvr}/trunk/src/"), true, 5)]),
        ),
    )
    .await;
    let s = session(&srv).await;
    let mut out = Vec::new();
    let err = s
        .get_file(&p("src"), None, &mut out, &CancelToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PathNotFound);
    assert!(requests(&srv).await.iter().all(|(m, _)| m != "GET"));
}

#[tokio::test]
async fn cancel_stops_a_slow_download() {
    let srv = server(Bulk::Prefer).await;
    mount(
        &srv,
        "PROPFIND",
        "/svn/repo/!svn/rvr/5/trunk/a.txt",
        xml(207, fixture("propfind/allprop-file.resp")),
    )
    .await;
    mount(
        &srv,
        "GET",
        "/svn/repo/!svn/rvr/5/trunk/a.txt",
        ResponseTemplate::new(200)
            .set_body_bytes(fixture("get/file.resp"))
            .set_delay(Duration::from_secs(30)),
    )
    .await;
    let s = session(&srv).await;
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        c2.cancel();
    });
    let started = std::time::Instant::now();
    let mut out = Vec::new();
    let err = s
        .get_file(&p("a.txt"), Some(Revnum(5)), &mut out, &cancel)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(out.is_empty());
}

#[tokio::test]
async fn log_streams_entries_newest_first() {
    let srv = server(Bulk::Prefer).await;
    Mock::given(method("REPORT"))
        .and(path("/svn/repo/!svn/rvr/5/trunk"))
        .and(body_string_contains(
            "<S:start-revision>5</S:start-revision>",
        ))
        .and(body_string_contains("<S:end-revision>1</S:end-revision>"))
        .and(body_string_contains("<S:discover-changed-paths/>"))
        .respond_with(xml(200, fixture("log/head-verbose.resp")))
        .mount(&srv)
        .await;
    let s = session(&srv).await;
    let req = LogRequest {
        paths: Vec::new(),
        start: None,
        end: None,
        limit: None,
        discover_changed_paths: true,
        strict_node_history: false,
    };
    let mut seen: Vec<LogEntry> = Vec::new();
    s.log(
        &req,
        &mut |e| {
            seen.push(e);
            Ok(())
        },
        &CancelToken::new(),
    )
    .await
    .unwrap();
    assert!(!seen.is_empty());
    assert!(seen.windows(2).all(|w| w[0].rev > w[1].rev));
    assert_eq!(seen[0].rev, Revnum(6));
    assert_eq!(seen[0].author.as_deref(), Some("alice"));
    assert!(
        seen[0]
            .changed_paths
            .iter()
            .any(|c| c.path.as_str() == "/trunk/newdir/n.txt")
    );

    // An error from the consumer stops the stream and is returned as is.
    let mut count = 0;
    let err = s
        .log(
            &req,
            &mut |_| {
                count += 1;
                Err(svn_types::SvnError::new(ErrorCode::Cancelled, "enough"))
            },
            &CancelToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.message, "enough");
    assert_eq!(count, 1);
}

#[tokio::test]
async fn get_locks_reads_the_report() {
    let srv = server(Bulk::Prefer).await;
    mount(
        &srv,
        "REPORT",
        "/svn/repo/trunk",
        xml(200, fixture("locks/get-locks-report.resp")),
    )
    .await;
    mount(
        &srv,
        "REPORT",
        "/svn/repo/trunk/gone",
        ResponseTemplate::new(404),
    )
    .await;
    let s = session(&srv).await;
    let cancel = CancelToken::new();
    let locks = s
        .get_locks(&RelPath::root(), Depth::Infinity, &cancel)
        .await
        .unwrap();
    assert_eq!(locks.len(), 1);
    assert_eq!(locks[0].path.as_str(), "/trunk/a.txt");
    assert_eq!(locks[0].owner, "alice");
    assert_eq!(locks[0].comment.as_deref(), Some("locking <it>"));
    assert!(
        s.get_locks(&p("gone"), Depth::Infinity, &cancel)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn lock_and_unlock() {
    let srv = server(Bulk::Prefer).await;
    Mock::given(method("LOCK"))
        .and(path("/svn/repo/trunk/a.txt"))
        .and(header("X-SVN-Version-Name", "3"))
        .respond_with(
            xml(200, fixture("locks/lock-ok.resp"))
                .insert_header("X-SVN-Creation-Date", "2026-10-13T09:45:12.389643Z")
                .insert_header("X-SVN-Lock-Owner", "alice"),
        )
        .mount(&srv)
        .await;
    mount(
        &srv,
        "LOCK",
        "/svn/repo/trunk/busy.txt",
        xml(423, fixture("locks/lock-conflict-423.resp")),
    )
    .await;
    Mock::given(method("UNLOCK"))
        .and(path("/svn/repo/trunk/a.txt"))
        .and(header(
            "Lock-Token",
            "<opaquelocktoken:a51c8c59-0dda-4d35-bb7f-b4ab1f3d2ee9>",
        ))
        .respond_with(ResponseTemplate::new(204))
        .mount(&srv)
        .await;
    mount(
        &srv,
        "UNLOCK",
        "/svn/repo/trunk/other.txt",
        ResponseTemplate::new(400),
    )
    .await;
    let s = session(&srv).await;
    let cancel = CancelToken::new();
    let lock = s
        .lock(
            &p("a.txt"),
            Some("locking <it>"),
            false,
            Some(Revnum(3)),
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(
        lock.token,
        "opaquelocktoken:a51c8c59-0dda-4d35-bb7f-b4ab1f3d2ee9"
    );
    assert_eq!(lock.path.as_str(), "/trunk/a.txt");
    assert_eq!(lock.owner, "alice");
    let err = s
        .lock(&p("busy.txt"), None, false, None, &cancel)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::LockedByOther);
    s.unlock(&p("a.txt"), Some(&lock.token), false, &cancel)
        .await
        .unwrap();
    let err = s
        .unlock(&p("other.txt"), Some("opaquelocktoken:x"), false, &cancel)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::LockTokenInvalid);
    let err = s
        .unlock(&p("a.txt"), None, false, &cancel)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::LockTokenInvalid);
}

#[tokio::test]
async fn break_lock_without_token_looks_the_token_up() {
    let srv = server(Bulk::Prefer).await;
    mount(
        &srv,
        "PROPFIND",
        "/svn/repo/trunk/a.txt",
        xml(207, fixture("propfind/get-lock.resp"))
            .insert_header("X-SVN-Creation-Date", "2026-10-13T09:45:12.389643Z")
            .insert_header("X-SVN-Lock-Owner", "alice"),
    )
    .await;
    Mock::given(method("UNLOCK"))
        .and(path("/svn/repo/trunk/a.txt"))
        .and(header("X-SVN-Options", "lock-break"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&srv)
        .await;
    let s = session(&srv).await;
    s.unlock(&p("a.txt"), None, true, &CancelToken::new())
        .await
        .unwrap();
    let all = srv.received_requests().await.unwrap();
    let unlock = all.iter().find(|r| r.method.as_str() == "UNLOCK").unwrap();
    let token = unlock.headers.get("Lock-Token").unwrap().to_str().unwrap();
    assert!(token.starts_with("<opaquelocktoken:"), "{token}");
}

/// Shared output buffer for a [`WindowApplier`].
#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Applier(WindowApplier<Cursor<Vec<u8>>, Shared>);

impl WindowSink for Applier {
    fn window(&mut self, w: Option<&TxDeltaWindow>) -> Result<()> {
        match w {
            Some(w) => self.0.apply_window(w),
            None => self.0.finish().map(|_| ()),
        }
    }
}

/// Rebuilds every revision's text, each delta applied to the previous text.
#[derive(Default)]
struct Rebuild {
    revs: Vec<FileRev>,
    texts: Vec<Shared>,
}

impl FileRevHandler for Rebuild {
    fn revision(&mut self, rev: FileRev) -> Result<Option<Box<dyn WindowSink>>> {
        let base = self
            .texts
            .last()
            .map(|t| t.0.lock().unwrap().clone())
            .unwrap_or_default();
        let out = Shared::default();
        self.texts.push(out.clone());
        self.revs.push(rev);
        Ok(Some(Box::new(Applier(WindowApplier::new(
            Cursor::new(base),
            out,
        )))))
    }
}

#[tokio::test]
async fn file_revs_rebuilds_each_revision() {
    let srv = server(Bulk::Prefer).await;
    Mock::given(method("REPORT"))
        .and(path("/svn/repo/!svn/rvr/9/trunk/a.txt"))
        .and(body_string_contains(
            "<S:start-revision>0</S:start-revision>",
        ))
        .and(body_string_contains("<S:end-revision>9</S:end-revision>"))
        .respond_with(xml(200, fixture("file_revs/blame.resp")))
        .mount(&srv)
        .await;
    let s = session(&srv).await;
    let mut h = Rebuild::default();
    s.file_revs(
        &p("a.txt"),
        None,
        Some(Revnum(9)),
        &mut h,
        &CancelToken::new(),
    )
    .await
    .unwrap();
    let revs: Vec<_> = h.revs.iter().map(|r| r.rev.0).collect();
    assert_eq!(revs, [2, 3, 6, 9]);
    assert_eq!(h.revs[0].path.as_str(), "/trunk/a.txt");
    assert!(
        h.revs[1]
            .rev_props
            .iter()
            .any(|p| p.name == "svn:log" && p.value.as_deref() == Some(&b"edit a.txt"[..]))
    );
    let first = h.texts[0].0.lock().unwrap().clone();
    assert_eq!(first, b"hello\nworld\n");
    let second = h.texts[1].0.lock().unwrap().clone();
    assert_eq!(second, b"hello\nworld\nmore\n");
    assert!(!h.texts.last().unwrap().0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reverse_file_revs_needs_the_capability() {
    let srv = server(Bulk::Prefer).await;
    let s = session(&srv).await;
    let mut h = Rebuild::default();
    let err = s
        .file_revs(
            &p("a.txt"),
            Some(Revnum(9)),
            Some(Revnum(1)),
            &mut h,
            &CancelToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ServerUnsupported);
    assert!(requests(&srv).await.iter().all(|(m, _)| m != "REPORT"));
}

#[tokio::test]
async fn get_file_returns_the_md5_and_whether_it_was_verified() {
    let srv = server(Bulk::Prefer).await;
    let content = fixture("get/file.resp");
    mount_file(&srv, &content).await;
    let s = session(&srv).await;
    let mut out = Vec::new();
    let meta = s
        .get_file(&p("a.txt"), None, &mut out, &CancelToken::new())
        .await
        .unwrap();
    let md5: [u8; 16] = md5::Md5::digest(&content).into();
    assert_eq!(meta.md5, Checksum::Md5(md5));
    assert!(meta.verified);
}

#[tokio::test]
async fn get_file_without_a_server_checksum_still_returns_one() {
    let srv = server(Bulk::Prefer).await;
    let props = String::from_utf8(fixture("propfind/allprop-file.resp"))
        .unwrap()
        .replace(
            "<lp2:md5-checksum>3e079718d4006e9d757934ce54439418</lp2:md5-checksum>",
            "",
        );
    assert!(!props.contains("md5-checksum"));
    mount(
        &srv,
        "PROPFIND",
        "/svn/repo/!svn/rvr/5/trunk/a.txt",
        xml(207, props),
    )
    .await;
    mount(
        &srv,
        "GET",
        "/svn/repo/!svn/rvr/5/trunk/a.txt",
        ResponseTemplate::new(200).set_body_bytes(b"anything\n".to_vec()),
    )
    .await;
    let s = session(&srv).await;
    let mut out = Vec::new();
    let meta = s
        .get_file(&p("a.txt"), None, &mut out, &CancelToken::new())
        .await
        .unwrap();
    assert_eq!(out, b"anything\n");
    let md5: [u8; 16] = md5::Md5::digest(b"anything\n").into();
    assert_eq!(meta.md5, Checksum::Md5(md5));
    assert!(!meta.verified);
}

/// Mounts the `Depth: 1` listing of `dir` at r7 with `children` as `(name, is_dir)`; a name
/// may repeat.
async fn mount_listing(srv: &MockServer, dir: &str, children: &[(&str, bool)]) {
    let rvr = "/svn/repo/!svn/rvr/7";
    let mut rs = vec![response(&format!("{rvr}/{dir}/"), true, 7)];
    for (name, is_dir) in children {
        let slash = if *is_dir { "/" } else { "" };
        rs.push(response(&format!("{rvr}/{dir}/{name}{slash}"), *is_dir, 7));
    }
    mount(
        srv,
        "PROPFIND",
        &format!("{rvr}/{dir}"),
        xml(207, multistatus(&rs)),
    )
    .await;
}

async fn session_with_limits(srv: &MockServer, limits: ListLimits) -> HttpRaSession {
    HttpRaSession::from_wire(wire(srv, 4).await.with_list_limits(limits)).unwrap()
}

#[tokio::test]
async fn list_infinity_ignores_repeated_entries() {
    let srv = server(Bulk::Prefer).await;
    // Every level repeats its subdirectory three times: without de-duplication the walk would
    // visit 3^depth directories.
    let dirs = [
        "trunk",
        "trunk/d",
        "trunk/d/d",
        "trunk/d/d/d",
        "trunk/d/d/d/d",
    ];
    for (i, dir) in dirs.iter().enumerate() {
        let last = i + 1 == dirs.len();
        let children: Vec<(&str, bool)> = if last {
            vec![("f", false), ("f", false)]
        } else {
            vec![("d", true), ("d", true), ("d", true), ("f", false)]
        };
        mount_listing(&srv, dir, &children).await;
    }
    let s = session(&srv).await;
    let entries = s
        .list(
            &RelPath::root(),
            Some(Revnum(7)),
            Depth::Infinity,
            &CancelToken::new(),
        )
        .await
        .unwrap();
    let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "d",
            "d/d",
            "d/d/d",
            "d/d/d/d",
            "d/d/d/d/f",
            "d/d/d/f",
            "d/d/f",
            "d/f",
            "f"
        ]
    );
    let propfinds = requests(&srv)
        .await
        .iter()
        .filter(|(m, _)| m == "PROPFIND")
        .count();
    assert_eq!(propfinds, dirs.len());
}

#[tokio::test]
async fn list_infinity_enforces_the_limits() {
    let srv = server(Bulk::Prefer).await;
    mount_listing(&srv, "trunk", &[("a", true), ("x", false), ("y", false)]).await;
    mount_listing(&srv, "trunk/a", &[("b", true)]).await;
    mount_listing(&srv, "trunk/a/b", &[("c", false)]).await;
    let list = |s: HttpRaSession| async move {
        s.list(
            &RelPath::root(),
            Some(Revnum(7)),
            Depth::Infinity,
            &CancelToken::new(),
        )
        .await
    };

    let all = list(session_with_limits(&srv, ListLimits::default()).await)
        .await
        .unwrap();
    assert_eq!(all.len(), 5);
    let exact = ListLimits {
        max_entries: 5,
        max_depth: 3,
    };
    assert_eq!(
        list(session_with_limits(&srv, exact).await)
            .await
            .unwrap()
            .len(),
        5
    );

    let few = ListLimits {
        max_entries: 4,
        ..ListLimits::default()
    };
    let err = list(session_with_limits(&srv, few).await)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::LimitExceeded);
    assert!(err.message.contains("4 entries"), "{}", err.message);

    let shallow = ListLimits {
        max_depth: 2,
        ..ListLimits::default()
    };
    let err = list(session_with_limits(&srv, shallow).await)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::LimitExceeded);
    assert!(err.message.contains("2 levels"), "{}", err.message);
}

/// One-level listings are capped too; `Files` counts children of every kind, since all of
/// them are read before the filter applies.
#[tokio::test]
async fn one_level_listings_enforce_the_entry_limit() {
    let srv = server(Bulk::Prefer).await;
    mount_listing(&srv, "trunk", &[("a", true), ("x", false), ("y", false)]).await;
    for depth in [Depth::Immediates, Depth::Files] {
        let list = |limits| {
            let srv = &srv;
            async move {
                session_with_limits(srv, limits)
                    .await
                    .list(
                        &RelPath::root(),
                        Some(Revnum(7)),
                        depth,
                        &CancelToken::new(),
                    )
                    .await
            }
        };
        let exact = ListLimits {
            max_entries: 3,
            ..ListLimits::default()
        };
        let expected = if depth == Depth::Files { 2 } else { 3 };
        assert_eq!(list(exact).await.unwrap().len(), expected, "{depth:?}");
        let few = ListLimits {
            max_entries: 2,
            ..ListLimits::default()
        };
        let err = list(few).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::LimitExceeded, "{depth:?}");
        assert!(err.message.contains("2 entries"), "{}", err.message);
    }
}

/// The cap is enforced while the answer streams in: an answer whose tail is broken still
/// fails on the cap (it would be `Corrupt` if the whole body were read before counting).
#[tokio::test]
async fn entry_limit_is_enforced_while_streaming() {
    let srv = server(Bulk::Prefer).await;
    let rvr = "/svn/repo/!svn/rvr/7";
    let mut rs = vec![response(&format!("{rvr}/trunk/"), true, 7)];
    // About 2 MB of entries, so the body arrives in many chunks before the broken tail.
    for i in 0..5000 {
        rs.push(response(&format!("{rvr}/trunk/f{i:05}"), false, 7));
    }
    let mut body = multistatus(&rs).into_bytes();
    body.truncate(body.len() - "</D:multistatus>".len());
    body.extend_from_slice(b"<D:response><D:href>broken");
    mount(&srv, "PROPFIND", &format!("{rvr}/trunk"), xml(207, body)).await;

    let s = session_with_limits(
        &srv,
        ListLimits {
            max_entries: 10,
            ..ListLimits::default()
        },
    )
    .await;
    let err = s
        .list(
            &RelPath::root(),
            Some(Revnum(7)),
            Depth::Immediates,
            &CancelToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::LimitExceeded, "{err:?}");
    // Control: without the cap the broken answer is reported as such.
    let err = session(&srv)
        .await
        .list(
            &RelPath::root(),
            Some(Revnum(7)),
            Depth::Immediates,
            &CancelToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Corrupt, "{err:?}");
}

#[tokio::test]
async fn get_locks_drops_locks_outside_the_requested_scope() {
    let srv = server(Bulk::Prefer).await;
    let lock = |path: &str| {
        format!(
            "<S:lock><S:path>{path}</S:path>\
             <S:token>opaquelocktoken:a51c8c59-0dda-4d35-bb7f-b4ab1f3d2ee9</S:token>\
             <S:creationdate>2026-10-13T09:45:12.389643Z</S:creationdate>\
             <S:owner>alice</S:owner></S:lock>"
        )
    };
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <S:get-locks-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">{}</S:get-locks-report>",
        [
            "/trunk/d/a.txt",
            "/trunk/d/sub/b.txt",
            "/trunk/other.txt",
            "/branches/x.txt",
            "/trunk/d2/c.txt",
        ]
        .map(lock)
        .concat()
    );
    mount(&srv, "REPORT", "/svn/repo/trunk/d", xml(200, body)).await;
    let s = session(&srv).await;
    let cancel = CancelToken::new();
    let paths = |locks: Vec<svn_types::LockInfo>| {
        locks
            .into_iter()
            .map(|l| l.path.as_str().to_owned())
            .collect::<Vec<_>>()
    };
    let all = s
        .get_locks(&p("d"), Depth::Infinity, &cancel)
        .await
        .unwrap();
    assert_eq!(paths(all), ["/trunk/d/a.txt", "/trunk/d/sub/b.txt"]);
    let near = s
        .get_locks(&p("d"), Depth::Immediates, &cancel)
        .await
        .unwrap();
    assert_eq!(paths(near), ["/trunk/d/a.txt"]);
}
