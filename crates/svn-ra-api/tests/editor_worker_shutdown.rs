//! `close_edit` / `abort_edit` must finish even while a `WindowSender` is still alive.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::time::Duration;

use bytes::Bytes;
use svn_ra_api::EditorWorker;
use svn_ra_api::fake::RecordingEditor;
use svn_types::{ErrorCode, PropChange, RelPath, TxDeltaWindow};

const LIMIT: Duration = Duration::from_secs(5);

fn p(s: &str) -> RelPath {
    RelPath::new(s).expect("rel path")
}

#[tokio::test(flavor = "multi_thread")]
async fn abort_edit_returns_while_a_window_sender_is_alive() {
    let (editor, log) = RecordingEditor::new();
    let handle = EditorWorker::spawn(Box::new(editor), 4);
    let root = handle.open_root(None).await.expect("root");
    let file = handle.add_file(&p("f"), root, None).await.expect("file");
    let sender = handle.apply_textdelta(file, None).await.expect("delta");

    tokio::time::timeout(LIMIT, handle.abort_edit())
        .await
        .expect("abort_edit must not hang")
        .expect("abort");
    assert!(log.is_aborted());

    let err = sender
        .send(Some(TxDeltaWindow::full_text(Bytes::from_static(b"late"))))
        .await
        .expect_err("closed");
    assert_eq!(err.code, ErrorCode::Cancelled);
}

#[tokio::test(flavor = "multi_thread")]
async fn close_edit_returns_while_a_window_sender_is_alive() {
    let (editor, log) = RecordingEditor::new();
    let handle = EditorWorker::spawn(Box::new(editor), 4);
    let root = handle.open_root(None).await.expect("root");
    let file = handle.add_file(&p("f"), root, None).await.expect("file");
    let sender = handle.apply_textdelta(file, None).await.expect("delta");
    sender.send(None).await.expect("end of delta");
    handle.close_file(file, None).await.expect("close file");
    handle.close_directory(root).await.expect("close dir");

    tokio::time::timeout(LIMIT, handle.close_edit())
        .await
        .expect("close_edit must not hang")
        .expect("close");
    assert!(log.is_closed());
    assert_eq!(
        sender.send(None).await.expect_err("closed").code,
        ErrorCode::Cancelled
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn failure_with_a_live_window_sender_does_not_leak_the_worker() {
    let (editor, _log) = RecordingEditor::new();
    let handle = EditorWorker::spawn(Box::new(editor.fail_at(4)), 4);
    let root = handle.open_root(None).await.expect("root");
    let file = handle.add_file(&p("f"), root, None).await.expect("file");
    let sender = handle.apply_textdelta(file, None).await.expect("delta");
    // Call 4 fails inside the worker; the next call observes the poisoned handle.
    let _ = sender
        .send(Some(TxDeltaWindow::full_text(Bytes::from_static(b"x"))))
        .await;
    let err = tokio::time::timeout(LIMIT, handle.close_edit())
        .await
        .expect("must not hang")
        .expect_err("poisoned");
    assert_eq!(err.code, ErrorCode::Internal);
    assert!(sender.send(None).await.is_err());
}

/// The reply channel of the panicking command is dropped while the worker unwinds; the handle
/// must still see `Internal`, never a "finished" `Cancelled`, however the threads interleave.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn editor_panic_is_always_internal_never_cancelled() {
    for i in 0..1000 {
        let (editor, _log) = RecordingEditor::new();
        let editor = editor.on_call(|n| {
            if n == 2 {
                panic!("boom");
            }
        });
        let handle = EditorWorker::spawn(Box::new(editor), 4);
        let root = handle.open_root(None).await.expect("root");
        let err = handle
            .add_directory(&p("d"), root, None)
            .await
            .expect_err("panicked");
        assert_eq!(
            err.code,
            ErrorCode::Internal,
            "iteration {i}: {}",
            err.message
        );
        let err = handle.close_edit().await.expect_err("poisoned");
        assert_eq!(
            err.code,
            ErrorCode::Internal,
            "iteration {i} (close): {}",
            err.message
        );
    }
}

/// Same race for commands that carry no reply: the failure must be visible to the next call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panic_in_a_queued_command_surfaces_as_internal_on_the_next_call() {
    for i in 0..500 {
        let (editor, _log) = RecordingEditor::new();
        let editor = editor.on_call(|n| {
            if n == 2 {
                panic!("boom");
            }
        });
        let handle = EditorWorker::spawn(Box::new(editor), 4);
        let root = handle.open_root(None).await.expect("root");
        // Queued without a reply; the worker panics while executing it.
        let _ = handle
            .change_dir_prop(
                root,
                PropChange {
                    name: "x".into(),
                    value: None,
                },
            )
            .await;
        let err = handle.close_edit().await.expect_err("panicked");
        assert_eq!(
            err.code,
            ErrorCode::Internal,
            "iteration {i}: {}",
            err.message
        );
    }
}
