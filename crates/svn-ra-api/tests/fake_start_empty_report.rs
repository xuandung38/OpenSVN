//! The fake drive for `start_empty` reports matches svn_repos (`reporter.c`, #151): a path
//! reported with its own `set_path` exists at its reported revision even when it or its parent is
//! `start_empty` (that only empties its children). So a deleted path gets `delete_entry`, and a
//! `start_empty` directory that still exists is opened, with every property sent.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use bytes::Bytes;
use svn_ra_api::fake::{EditorEvent, FakeChange, FakeRaSession, RecordingEditor};
use svn_ra_api::{RaSession, ReportFn, UpdateMode, UpdateRequest};
use svn_types::{CancelToken, Depth, PropChange, RelPath, Revnum};

fn p(s: &str) -> RelPath {
    RelPath::new(s).expect("rel path")
}

fn put(path: &str, content: &str) -> FakeChange {
    FakeChange::PutFile(p(path), Bytes::from(content.to_owned()))
}

fn prop(path: &str, name: &str, value: &str) -> FakeChange {
    FakeChange::SetProp(
        p(path),
        PropChange {
            name: name.into(),
            value: Some(Bytes::from(value.to_owned())),
        },
    )
}

/// r1: `d` (with a property) and `keep`; r2: a file added to each; r3: `d` deleted.
fn session() -> FakeRaSession {
    let s = FakeRaSession::new("https://svn.test.local/repos/demo", "uuid-1").expect("session");
    s.seed(
        "alice",
        "r1",
        &[
            put("d/in.txt", "in"),
            prop("d", "color", "red"),
            put("keep/k.txt", "k"),
        ],
    )
    .expect("r1");
    s.seed(
        "alice",
        "r2",
        &[put("d/new.txt", "new"), put("keep/k2.txt", "k2")],
    )
    .expect("r2");
    s.seed("alice", "r3", &[FakeChange::Delete(p("d"))])
        .expect("r3");
    s
}

fn req(rev: u64) -> UpdateRequest {
    UpdateRequest {
        revision: Some(Revnum(rev)),
        target: RelPath::root(),
        depth: Depth::Unknown,
        mode: UpdateMode::Update,
        send_copyfrom: false,
        ignore_ancestry: false,
    }
}

/// What a working copy left by an interrupted update to r2 reports (svn's crawler): the root and
/// `d` are `incomplete` (`start_empty`), so every child is reported, `d/in.txt` still at r1.
fn interrupted_report() -> ReportFn {
    Box::new(|r| {
        r.set_path(&RelPath::root(), Revnum(2), Depth::Infinity, true, None)?;
        r.set_path(&p("d"), Revnum(2), Depth::Infinity, true, None)?;
        r.set_path(&p("d/in.txt"), Revnum(1), Depth::Infinity, false, None)?;
        r.set_path(&p("keep"), Revnum(1), Depth::Infinity, false, None)?;
        r.finish()
    })
}

async fn drive(rev: u64, report: ReportFn) -> Vec<EditorEvent> {
    let (editor, log) = RecordingEditor::new();
    session()
        .do_update(req(rev), report, Box::new(editor), &CancelToken::new())
        .await
        .expect("update");
    assert!(log.violations().is_empty(), "{:?}", log.violations());
    log.events()
}

#[tokio::test(flavor = "multi_thread")]
async fn deleted_directory_reported_start_empty_gets_delete_entry() {
    let ev = drive(3, interrupted_report()).await;
    assert!(
        ev.contains(&EditorEvent::DeleteEntry(p("d"))),
        "no delete_entry for d: {ev:?}"
    );
    assert!(
        !ev.iter().any(|e| matches!(
            e,
            EditorEvent::OpenDirectory(x, _) | EditorEvent::AddDirectory(x, _) if x == &p("d")
        )),
        "{ev:?}"
    );
    // The root is `start_empty` too: `keep` (reported on its own) is sent as an edit.
    assert!(
        ev.iter()
            .any(|e| matches!(e, EditorEvent::OpenDirectory(x, _) if x == &p("keep"))),
        "{ev:?}"
    );
    assert!(
        ev.iter()
            .any(|e| matches!(e, EditorEvent::AddFile(x, _) if x == &p("keep/k2.txt"))),
        "{ev:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn child_reported_below_a_start_empty_parent_gets_delete_entry() {
    // `d` is complete at r1 below an incomplete root: the server knows `d` from its own entry.
    let report: ReportFn = Box::new(|r| {
        r.set_path(&RelPath::root(), Revnum(2), Depth::Infinity, true, None)?;
        r.set_path(&p("d"), Revnum(1), Depth::Infinity, false, None)?;
        r.set_path(&p("keep"), Revnum(2), Depth::Infinity, false, None)?;
        r.finish()
    });
    let ev = drive(3, report).await;
    assert!(
        ev.contains(&EditorEvent::DeleteEntry(p("d"))),
        "no delete_entry for d: {ev:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn start_empty_directory_that_still_exists_is_opened_with_all_properties() {
    let ev = drive(2, interrupted_report()).await;
    let d = ev
        .iter()
        .find_map(|e| match e {
            EditorEvent::OpenDirectory(x, id) if x == &p("d") => Some(*id),
            _ => None,
        })
        .unwrap_or_else(|| panic!("d is not opened: {ev:?}"));
    assert!(
        !ev.iter()
            .any(|e| matches!(e, EditorEvent::AddDirectory(x, _) if x == &p("d"))),
        "{ev:?}"
    );
    // Properties are diffed against none: the unchanged `color` is sent again.
    assert!(
        ev.contains(&EditorEvent::ChangeDirProp(d, "color".into())),
        "{ev:?}"
    );
    // Children of a `start_empty` directory: the reported one is left alone when it did not
    // change (it is known at its own revision), the others are adds.
    assert!(
        !ev.iter().any(|e| matches!(
            e,
            EditorEvent::OpenFile(x, _) | EditorEvent::AddFile(x, _) if x == &p("d/in.txt")
        )),
        "{ev:?}"
    );
    assert!(
        ev.iter()
            .any(|e| matches!(e, EditorEvent::AddFile(x, _) if x == &p("d/new.txt"))),
        "{ev:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_start_empty_directory_is_still_opened() {
    // `keep` holds nothing new at r2 for a working copy at r2, but reported `start_empty` it is
    // opened anyway, so the client can complete it.
    let report: ReportFn = Box::new(|r| {
        r.set_path(&RelPath::root(), Revnum(2), Depth::Infinity, false, None)?;
        r.set_path(&p("keep"), Revnum(2), Depth::Infinity, true, None)?;
        r.set_path(&p("keep/k.txt"), Revnum(2), Depth::Infinity, false, None)?;
        r.set_path(&p("keep/k2.txt"), Revnum(2), Depth::Infinity, false, None)?;
        r.finish()
    });
    let ev = drive(2, report).await;
    assert!(
        ev.iter()
            .any(|e| matches!(e, EditorEvent::OpenDirectory(x, _) if x == &p("keep"))),
        "{ev:?}"
    );
}
