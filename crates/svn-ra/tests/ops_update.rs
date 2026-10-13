//! Update driver against a scripted server: send-all and skeleton answers, switch, status,
//! hostile answers and cancellation.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod ra_write_support;

use std::time::{Duration, Instant};

use ra_write_support::{Bulk, ROOT, fixture, requests, server, wire};
use svn_ra::ops::update::run_update;
use svn_ra_api::fake::{RecordingEditor, RecordingLog};
use svn_ra_api::{ReportFn, UpdateMode, UpdateRequest};
use svn_types::{CancelToken, Depth, ErrorCode, RelPath, RepoUrl, Revnum};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn p(s: &str) -> RelPath {
    RelPath::new(s).expect("path")
}

fn report() -> ReportFn {
    Box::new(|r| {
        r.set_path(&RelPath::root(), Revnum(0), Depth::Infinity, true, None)?;
        r.finish()
    })
}

fn request(mode: UpdateMode) -> UpdateRequest {
    UpdateRequest {
        revision: None,
        target: RelPath::root(),
        depth: Depth::Infinity,
        mode,
        send_copyfrom: false,
        ignore_ancestry: false,
    }
}

async fn serve_report(server: &MockServer, body: Vec<u8>) {
    Mock::given(method("REPORT"))
        .and(path("/svn/repo/!svn/me"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(server)
        .await;
}

fn expected_tree() -> Vec<(&'static str, &'static str)> {
    vec![
        ("a.txt", "hello\nworld\nmore\n"),
        ("src/b.txt", "second file\n"),
        ("src/deep/c.cfg", "x=1\n"),
        ("unicodé file.txt", "café €\n"),
    ]
}

fn assert_tree(log: &RecordingLog) {
    assert!(log.violations().is_empty(), "{:?}", log.violations());
    assert!(log.is_closed());
    let contents = log.file_contents();
    for (name, text) in expected_tree() {
        assert_eq!(
            contents.get(&p(name)).map(Vec::as_slice),
            Some(text.as_bytes()),
            "{name}"
        );
    }
}

#[tokio::test]
async fn send_all_checkout_drives_the_editor() {
    let srv = server(Bulk::Prefer).await;
    serve_report(&srv, fixture("update/checkout-send-all.resp")).await;
    let w = wire(&srv, 4).await;
    let (editor, log) = RecordingEditor::new();
    run_update(
        &w,
        request(UpdateMode::Update),
        report(),
        Box::new(editor),
        &CancelToken::new(),
    )
    .await
    .expect("update");
    assert_tree(&log);
    // Only the report was needed: no per-file requests.
    assert_eq!(requests(&srv).await.len(), 1);
}

#[tokio::test]
async fn skeleton_checkout_downloads_files_in_parallel() {
    let srv = server(Bulk::Off).await;
    serve_report(&srv, fixture("update/checkout-skelta.resp")).await;
    for (href, text) in [
        ("3/trunk/a.txt", "hello\nworld\nmore\n"),
        ("2/trunk/src/b.txt", "second file\n"),
        ("2/trunk/src/deep/c.cfg", "x=1\n"),
        ("2/trunk/unicod%C3%A9%20file.txt", "café €\n"),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("{ROOT}/!svn/rvr/{href}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(text))
            .mount(&srv)
            .await;
    }
    let w = wire(&srv, 4).await;
    let (editor, log) = RecordingEditor::new();
    run_update(
        &w,
        request(UpdateMode::Update),
        report(),
        Box::new(editor),
        &CancelToken::new(),
    )
    .await
    .expect("update");
    assert_tree(&log);
    let gets = requests(&srv)
        .await
        .into_iter()
        .filter(|(m, _)| m == "GET")
        .count();
    assert_eq!(gets, 4);
}

#[tokio::test]
async fn skeleton_with_single_connection_uses_send_all_when_allowed() {
    // Without the bulk header and with inline props the driver would pick skeleton mode.
    let srv = server(Bulk::Absent).await;
    Mock::given(method("REPORT"))
        .and(body_string_contains("send-all=\"true\""))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(fixture("update/checkout-send-all.resp")),
        )
        .mount(&srv)
        .await;
    let w = wire(&srv, 1).await;
    let (editor, log) = RecordingEditor::new();
    run_update(
        &w,
        request(UpdateMode::Update),
        report(),
        Box::new(editor),
        &CancelToken::new(),
    )
    .await
    .expect("update");
    assert_tree(&log);
}

#[tokio::test]
async fn skeleton_needs_a_second_connection_when_bulk_is_forbidden() {
    let srv = server(Bulk::Off).await;
    let w = wire(&srv, 1).await;
    let (editor, log) = RecordingEditor::new();
    let err = run_update(
        &w,
        request(UpdateMode::Update),
        report(),
        Box::new(editor),
        &CancelToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::ServerUnsupported);
    assert!(log.is_aborted());
    assert!(requests(&srv).await.is_empty(), "nothing may be sent");
}

#[tokio::test]
async fn status_drive_skips_text() {
    let srv = server(Bulk::Prefer).await;
    Mock::given(method("REPORT"))
        .and(body_string_contains("<S:text-deltas>no</S:text-deltas>"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(fixture("update/checkout-send-all.resp")),
        )
        .mount(&srv)
        .await;
    let w = wire(&srv, 4).await;
    let (editor, log) = RecordingEditor::new();
    run_update(
        &w,
        request(UpdateMode::Status),
        report(),
        Box::new(editor),
        &CancelToken::new(),
    )
    .await
    .expect("status");
    assert!(log.violations().is_empty());
    assert!(
        log.file_contents().values().all(Vec::is_empty),
        "a status drive must not deliver text"
    );
}

#[tokio::test]
async fn switch_sends_destination_and_target() {
    let srv = server(Bulk::Prefer).await;
    Mock::given(method("REPORT"))
        .and(body_string_contains("<S:dst-path>"))
        .and(body_string_contains("/svn/repo/branches/b1"))
        .and(body_string_contains(
            "<S:update-target>src</S:update-target>",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(fixture("update/checkout-send-all.resp")),
        )
        .mount(&srv)
        .await;
    let w = wire(&srv, 4).await;
    let (editor, log) = RecordingEditor::new();
    let mut req = request(UpdateMode::Switch(
        RepoUrl::parse(&format!("{}{ROOT}/branches/b1", srv.uri())).unwrap(),
    ));
    req.target = p("src");
    run_update(&w, req, report(), Box::new(editor), &CancelToken::new())
        .await
        .expect("switch");
    assert!(log.violations().is_empty());
}

#[tokio::test]
async fn switch_to_a_foreign_origin_is_refused_before_any_request() {
    let srv = server(Bulk::Prefer).await;
    let w = wire(&srv, 4).await;
    let (editor, log) = RecordingEditor::new();
    let req = request(UpdateMode::Switch(
        RepoUrl::parse("http://evil.test.local/svn/repo/x").unwrap(),
    ));
    let err = run_update(&w, req, report(), Box::new(editor), &CancelToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::UnsafePath);
    assert!(log.is_aborted());
    assert!(requests(&srv).await.is_empty());
}

#[tokio::test]
async fn hostile_href_to_another_origin_stops_the_update() {
    let srv = server(Bulk::Off).await;
    let body = String::from_utf8(fixture("update/checkout-skelta.resp"))
        .unwrap()
        .replace(
            "/svn/repo/!svn/rvr/3/trunk/a.txt",
            "http://evil.test.local/steal",
        );
    serve_report(&srv, body.into_bytes()).await;
    let w = wire(&srv, 4).await;
    let (editor, log) = RecordingEditor::new();
    let err = run_update(
        &w,
        request(UpdateMode::Update),
        report(),
        Box::new(editor),
        &CancelToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::UnsafePath);
    assert!(log.is_aborted());
    assert!(
        requests(&srv).await.iter().all(|(m, _)| m != "GET"),
        "no download may start"
    );
}

#[tokio::test]
async fn unsafe_node_name_stops_the_update() {
    let srv = server(Bulk::Prefer).await;
    let body = String::from_utf8(fixture("update/checkout-send-all.resp"))
        .unwrap()
        .replace("name=\"docs\"", "name=\"..\"");
    serve_report(&srv, body.into_bytes()).await;
    let w = wire(&srv, 4).await;
    let (editor, log) = RecordingEditor::new();
    let err = run_update(
        &w,
        request(UpdateMode::Update),
        report(),
        Box::new(editor),
        &CancelToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::UnsafePath);
    assert!(log.is_aborted());
}

#[tokio::test]
async fn truncated_report_is_corrupt_and_aborts_the_editor() {
    let srv = server(Bulk::Prefer).await;
    let full = fixture("update/checkout-send-all.resp");
    serve_report(&srv, full[..full.len() / 2].to_vec()).await;
    let w = wire(&srv, 4).await;
    let (editor, log) = RecordingEditor::new();
    let err = run_update(
        &w,
        request(UpdateMode::Update),
        report(),
        Box::new(editor),
        &CancelToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::Corrupt);
    assert!(log.is_aborted());
    assert!(!log.is_closed());
}

#[tokio::test]
async fn failed_download_aborts_the_drive() {
    let srv = server(Bulk::Off).await;
    serve_report(&srv, fixture("update/checkout-skelta.resp")).await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&srv)
        .await;
    let w = wire(&srv, 4).await;
    let (editor, log) = RecordingEditor::new();
    let err = run_update(
        &w,
        request(UpdateMode::Update),
        report(),
        Box::new(editor),
        &CancelToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::PathNotFound);
    assert!(log.is_aborted());
}

#[tokio::test]
async fn server_error_on_the_report_is_mapped() {
    let srv = server(Bulk::Prefer).await;
    Mock::given(method("REPORT"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&srv)
        .await;
    let w = wire(&srv, 4).await;
    let (editor, log) = RecordingEditor::new();
    let err = run_update(
        &w,
        request(UpdateMode::Update),
        report(),
        Box::new(editor),
        &CancelToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::Forbidden);
    assert!(log.is_aborted());
}

#[tokio::test]
async fn report_function_errors_abort_the_editor() {
    let srv = server(Bulk::Prefer).await;
    let w = wire(&srv, 4).await;
    let (editor, log) = RecordingEditor::new();
    let failing: ReportFn = Box::new(|_| {
        Err(svn_types::SvnError::new(
            ErrorCode::WcNeedsCleanup,
            "wc is broken",
        ))
    });
    let err = run_update(
        &w,
        request(UpdateMode::Update),
        failing,
        Box::new(editor),
        &CancelToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::WcNeedsCleanup);
    assert!(log.is_aborted());
    let unfinished: ReportFn = Box::new(|_| Ok(()));
    let (editor, _log) = RecordingEditor::new();
    let err = run_update(
        &w,
        request(UpdateMode::Update),
        unfinished,
        Box::new(editor),
        &CancelToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::Internal);
}

#[tokio::test]
async fn cancel_while_waiting_for_the_report_is_prompt() {
    let srv = server(Bulk::Prefer).await;
    Mock::given(method("REPORT"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(10))
                .set_body_bytes(fixture("update/checkout-send-all.resp")),
        )
        .mount(&srv)
        .await;
    let w = wire(&srv, 4).await;
    let (editor, log) = RecordingEditor::new();
    let cancel = CancelToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        trigger.cancel();
    });
    let started = Instant::now();
    let err = run_update(
        &w,
        request(UpdateMode::Update),
        report(),
        Box::new(editor),
        &cancel,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(log.is_aborted());
}

#[tokio::test]
async fn cancel_during_downloads_aborts_quickly() {
    let srv = server(Bulk::Off).await;
    serve_report(&srv, fixture("update/checkout-skelta.resp")).await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(10))
                .set_body_string("late"),
        )
        .mount(&srv)
        .await;
    let w = wire(&srv, 4).await;
    let (editor, log) = RecordingEditor::new();
    let cancel = CancelToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        trigger.cancel();
    });
    let started = Instant::now();
    let err = run_update(
        &w,
        request(UpdateMode::Update),
        report(),
        Box::new(editor),
        &cancel,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::Cancelled);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(log.is_aborted());
}
