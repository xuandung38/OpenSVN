//! Spike: a checkout of a three-level tree drives a synchronous editor through the editor worker
//! with late `close_file`, and the recording editor confirms the ordering rules.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use bytes::Bytes;
use svn_ra_api::fake::{
    EditorEvent, FakeChange, FakeRaSession, NullReporter, RecordingEditor, RecordingLog,
};
use svn_ra_api::{EditorWorker, RaSession, ReportFn, UpdateMode, UpdateRequest};
use svn_types::{
    CancelToken, DeltaEditor, Depth, DirId, ErrorCode, PropChange, RelPath, Revnum, TxDeltaWindow,
};

fn p(s: &str) -> RelPath {
    RelPath::new(s).expect("rel path")
}

fn put(path: &str, content: &str) -> FakeChange {
    FakeChange::PutFile(p(path), Bytes::from(content.to_owned()))
}

fn session() -> FakeRaSession {
    let s = FakeRaSession::new("https://svn.test.local/repos/demo", "uuid-1")
        .expect("session")
        .with_max_open_files(3);
    s.seed(
        "alice",
        "initial layout",
        &[
            put("trunk/a.txt", "alpha"),
            put("trunk/b.txt", "bravo"),
            put("trunk/docs/readme.md", "# readme"),
            put("trunk/src/lib.rs", "pub fn lib() {}"),
            put("trunk/src/mod/x.rs", "x"),
            put("trunk/src/mod/y.rs", "y"),
            put("trunk/src/mod/z.rs", "z"),
            FakeChange::SetProp(
                p("trunk/a.txt"),
                PropChange {
                    name: "svn:eol-style".into(),
                    value: Some("native".into()),
                },
            ),
        ],
    )
    .expect("seed");
    s
}

fn update_req(rev: Option<Revnum>) -> UpdateRequest {
    UpdateRequest {
        revision: rev,
        target: RelPath::root(),
        depth: Depth::Infinity,
        mode: UpdateMode::Update,
        send_copyfrom: false,
        ignore_ancestry: false,
    }
}

fn report_from(rev: Revnum, start_empty: bool) -> ReportFn {
    Box::new(move |r| {
        r.set_path(&RelPath::root(), rev, Depth::Infinity, start_empty, None)?;
        r.finish()
    })
}

fn add_pos(ev: &[EditorEvent], id: svn_types::FileId) -> usize {
    ev.iter()
        .position(|e| matches!(e, EditorEvent::AddFile(_, i) if *i == id))
        .expect("add")
}

fn close_pos(ev: &[EditorEvent], id: svn_types::FileId) -> usize {
    ev.iter()
        .position(|e| *e == EditorEvent::CloseFile(id))
        .expect("close")
}

async fn checkout(s: &FakeRaSession) -> RecordingLog {
    let (editor, log) = RecordingEditor::new();
    s.do_update(
        update_req(None),
        report_from(Revnum(0), true),
        Box::new(editor),
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    log
}

#[tokio::test(flavor = "multi_thread")]
async fn checkout_three_level_tree_with_late_close_file() {
    let s = session();
    let log = checkout(&s).await;
    let ev = log.events();

    assert!(
        log.violations().is_empty(),
        "violations: {:?}",
        log.violations()
    );
    assert!(log.is_closed() && !log.is_aborted());
    assert_eq!(ev.first(), Some(&EditorEvent::SetTargetRevision(Revnum(1))));
    assert_eq!(ev.last(), Some(&EditorEvent::CloseEdit));

    // Content arrived intact.
    let contents = log.file_contents();
    assert_eq!(contents.len(), 7);
    assert_eq!(contents[&p("trunk/src/mod/y.rs")], b"y");
    assert_eq!(contents[&p("trunk/a.txt")], b"alpha");

    // Rule 1: at least one file is closed after a later file was added.
    let files: Vec<_> = ev
        .iter()
        .filter_map(|e| {
            if let EditorEvent::AddFile(_, id) = e {
                Some(*id)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(files.len(), 7);
    let late = files.iter().enumerate().any(|(i, f)| {
        files[i + 1..]
            .iter()
            .any(|later| close_pos(&ev, *f) > add_pos(&ev, *later))
    });
    assert!(
        late,
        "expected a close_file delivered after a later sibling was added: {ev:#?}"
    );

    // Rule 2: every directory closes after the files beneath it.
    for e in &ev {
        if let EditorEvent::AddDirectory(path, id) = e {
            let close_dir = ev
                .iter()
                .position(|x| *x == EditorEvent::CloseDirectory(*id))
                .expect("close dir");
            for fe in &ev {
                if let EditorEvent::AddFile(fp, fid) = fe
                    && fp.starts_with(path)
                {
                    assert!(close_pos(&ev, *fid) < close_dir, "{fp} closed after {path}");
                }
            }
        }
    }

    // Rule 3: one text delta per file, before its close.
    for f in files {
        let n = ev
            .iter()
            .filter(|e| **e == EditorEvent::ApplyTextDelta(f))
            .count();
        assert_eq!(n, 1);
    }
    // Properties travel with the file.
    assert!(
        ev.iter()
            .any(|e| matches!(e, EditorEvent::ChangeFileProp(_, n) if n == "svn:eol-style"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn incremental_update_sends_only_differences() {
    let s = session();
    s.seed(
        "bob",
        "change things",
        &[
            put("trunk/a.txt", "alpha v2"),
            FakeChange::Delete(p("trunk/docs/readme.md")),
            put("trunk/src/new.rs", "new"),
        ],
    )
    .expect("seed r2");

    let (editor, log) = RecordingEditor::new();
    s.do_update(
        update_req(None),
        report_from(Revnum(1), false),
        Box::new(editor),
        &CancelToken::new(),
    )
    .await
    .expect("update");
    let ev = log.events();

    assert!(log.violations().is_empty(), "{:?}", log.violations());
    assert!(ev.contains(&EditorEvent::SetTargetRevision(Revnum(2))));
    assert!(ev.contains(&EditorEvent::DeleteEntry(p("trunk/docs/readme.md"))));
    assert!(
        ev.iter()
            .any(|e| matches!(e, EditorEvent::OpenFile(path, _) if path == &p("trunk/a.txt")))
    );
    assert!(
        ev.iter()
            .any(|e| matches!(e, EditorEvent::AddFile(path, _) if path == &p("trunk/src/new.rs")))
    );
    // Untouched subtree stays closed.
    assert!(
        !ev.iter().any(
            |e| matches!(e, EditorEvent::OpenDirectory(path, _) if path == &p("trunk/src/mod"))
        )
    );
    assert!(
        !ev.iter()
            .any(|e| matches!(e, EditorEvent::OpenFile(path, _) if path == &p("trunk/b.txt")))
    );
    let contents = log.file_contents();
    assert_eq!(contents[&p("trunk/a.txt")], b"alpha v2");
    assert!(!contents.contains_key(&p("trunk/b.txt")));
}

#[tokio::test(flavor = "multi_thread")]
async fn status_mode_sends_no_text() {
    let s = session();
    let (editor, log) = RecordingEditor::new();
    let mut req = update_req(None);
    req.mode = UpdateMode::Status;
    s.do_update(
        req,
        report_from(Revnum(0), true),
        Box::new(editor),
        &CancelToken::new(),
    )
    .await
    .expect("status");
    assert!(
        !log.events()
            .iter()
            .any(|e| matches!(e, EditorEvent::ApplyTextDelta(_)))
    );
    assert!(log.is_closed());
}

#[tokio::test(flavor = "multi_thread")]
async fn update_of_a_subdirectory_target() {
    let s = session();
    let (editor, log) = RecordingEditor::new();
    let mut req = update_req(None);
    req.target = p("trunk/src");
    let report: ReportFn = Box::new(|r| {
        r.set_path(&RelPath::root(), Revnum(0), Depth::Infinity, true, None)?;
        r.finish()
    });
    s.do_update(req, report, Box::new(editor), &CancelToken::new())
        .await
        .expect("update");
    let contents = log.file_contents();
    assert_eq!(
        contents.len(),
        4,
        "only files under trunk/src: {contents:?}"
    );
    assert!(log.violations().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_mid_drive_aborts_the_editor() {
    let s = session();
    let token = CancelToken::new();
    let t = token.clone();
    let (editor, log) = RecordingEditor::new();
    let editor = editor.on_call(move |n| {
        if n == 8 {
            t.cancel();
        }
    });
    let err = s
        .do_update(
            update_req(None),
            report_from(Revnum(0), true),
            Box::new(editor),
            &token,
        )
        .await
        .expect_err("cancelled");
    assert_eq!(err.code, ErrorCode::Cancelled);
    assert!(log.is_aborted() && !log.is_closed());
    assert!(log.violations().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn editor_failure_aborts_and_surfaces_the_error() {
    let s = session();
    let (editor, log) = RecordingEditor::new();
    let err = s
        .do_update(
            update_req(None),
            report_from(Revnum(0), true),
            Box::new(editor.fail_at(9)),
            &CancelToken::new(),
        )
        .await
        .expect_err("editor failed");
    assert_eq!(err.code, ErrorCode::Internal);
    assert!(err.message.contains("injected"));
    assert!(log.is_aborted() && !log.is_closed());
}

#[tokio::test(flavor = "multi_thread")]
async fn report_failure_never_touches_the_editor() {
    let s = session();
    let (editor, log) = RecordingEditor::new();
    let report: ReportFn = Box::new(|_| {
        Err(svn_types::SvnError::new(
            ErrorCode::Internal,
            "report broke",
        ))
    });
    let err = s
        .do_update(
            update_req(None),
            report,
            Box::new(editor),
            &CancelToken::new(),
        )
        .await
        .expect_err("fails");
    assert_eq!(err.message, "report broke");
    assert!(log.events().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_handle_aborts_the_editor() {
    let (editor, log) = RecordingEditor::new();
    let handle = EditorWorker::spawn(Box::new(editor), 4);
    handle.open_root(None).await.expect("root");
    drop(handle);
    for _ in 0..100 {
        if log.is_aborted() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("worker did not abort the editor after the handle was dropped");
}

#[tokio::test(flavor = "multi_thread")]
async fn editor_panic_becomes_an_internal_error() {
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
    assert_eq!(err.code, ErrorCode::Internal);
    let err = handle.close_edit().await.expect_err("poisoned");
    assert_eq!(err.code, ErrorCode::Internal);
}

#[tokio::test(flavor = "multi_thread")]
async fn bounded_channel_keeps_many_queued_commands_in_order() {
    let (editor, log) = RecordingEditor::new();
    let handle = EditorWorker::spawn(Box::new(editor), 2);
    let root = handle.open_root(None).await.expect("root");
    for i in 0..200 {
        handle
            .change_dir_prop(
                root,
                PropChange {
                    name: format!("p{i}"),
                    value: None,
                },
            )
            .await
            .expect("prop");
    }
    handle.close_directory(root).await.expect("close dir");
    handle.close_edit().await.expect("close edit");
    let names: Vec<String> = log
        .events()
        .into_iter()
        .filter_map(|e| {
            if let EditorEvent::ChangeDirProp(_, n) = e {
                Some(n)
            } else {
                None
            }
        })
        .collect();
    let expected: Vec<String> = (0..200).map(|i| format!("p{i}")).collect();
    assert_eq!(names, expected);
}

#[test]
fn recording_editor_rejects_rule_violations() {
    // Rule 2: directory closed while a file below it is still open.
    let (mut e, log) = RecordingEditor::new();
    let root = e.open_root(None).expect("root");
    let f = e.add_file(&p("f"), root, None).expect("file");
    assert!(e.close_directory(root).is_err());
    assert_eq!(log.violations().len(), 1);
    e.close_file(f, None).expect("late close is fine");
    e.close_directory(root).expect("now ok");
    e.close_edit().expect("edit");

    // Rule 3: second text delta, and close before the delta ended.
    let (mut e, log) = RecordingEditor::new();
    let root = e.open_root(None).expect("root");
    let f = e.add_file(&p("f"), root, None).expect("file");
    let mut sink = e.apply_textdelta(f, None).expect("delta");
    assert!(e.apply_textdelta(f, None).is_err());
    assert!(e.close_file(f, None).is_err());
    sink.window(Some(&TxDeltaWindow::full_text(Bytes::from_static(b"hi"))))
        .expect("window");
    sink.window(None).expect("end");
    assert!(
        sink.window(None).is_err(),
        "window after the end of the delta"
    );
    e.close_file(f, None).expect("close");
    assert_eq!(log.violations().len(), 3);

    // Depth-first: no child under a directory that is not the innermost open one.
    let (mut e, _) = RecordingEditor::new();
    let root = e.open_root(None).expect("root");
    let _a = e.add_directory(&p("a"), root, None).expect("a");
    assert!(e.add_file(&p("f"), root, None).is_err());
    assert!(e.close_directory(root).is_err());
}

#[test]
fn recording_editor_allows_abort_at_any_time_and_blocks_later_calls() {
    let (mut e, log) = RecordingEditor::new();
    let root = e.open_root(None).expect("root");
    let _f = e.add_file(&p("f"), root, None).expect("file");
    e.abort_edit().expect("abort with open handles");
    assert!(log.is_aborted());
    assert!(e.add_file(&p("g"), DirId(1), None).is_err());
    e.abort_edit().expect("abort is repeatable");
    assert!(e.close_edit().is_err());
}

#[test]
fn null_reporter_accepts_everything() {
    use svn_types::Reporter;
    let mut r = NullReporter;
    r.set_path(&RelPath::root(), Revnum(1), Depth::Infinity, false, None)
        .expect("set");
    r.delete_path(&p("x")).expect("delete");
    r.finish().expect("finish");
    r.abort().expect("abort");
}

#[test]
fn recording_editor_checks_path_against_parent_and_target_revision_order() {
    let (mut e, log) = RecordingEditor::new();
    e.set_target_revision(Revnum(3)).expect("before open_root");
    let root = e.open_root(None).expect("root");
    assert!(
        e.add_file(&p("x/y/z"), root, None).is_err(),
        "not a child of the root"
    );
    let dir = e.add_directory(&p("a"), root, None).expect("a");
    assert!(
        e.add_file(&p("b/f"), dir, None).is_err(),
        "wrong parent directory"
    );
    assert!(
        e.delete_entry(&p("c"), None, dir).is_err(),
        "sibling of the parent, not a child"
    );
    e.add_file(&p("a/f"), dir, None).expect("correct child");
    assert!(e.set_target_revision(Revnum(4)).is_err(), "after open_root");
    assert_eq!(log.violations().len(), 4);
}
