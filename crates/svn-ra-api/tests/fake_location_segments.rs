//! `FakeRaSession::get_location_segments`: node history through copies, like
//! `svn_repos_node_location_segments`.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use bytes::Bytes;
use svn_ra_api::RaSession;
use svn_ra_api::fake::{FakeChange, FakeRaSession};
use svn_types::{CancelToken, ErrorCode, RelPath, RepoUrl, Revnum};

fn p(s: &str) -> RelPath {
    RelPath::new(s).unwrap()
}

fn put(path: &str, text: &str) -> FakeChange {
    FakeChange::PutFile(p(path), Bytes::from(text.to_owned()))
}

fn copy(from: &str, rev: u64, to: &str) -> FakeChange {
    FakeChange::Copy {
        from: p(from),
        rev: Revnum(rev),
        to: p(to),
    }
}

type Seg = (Option<String>, u64, u64);

async fn segments(
    s: &FakeRaSession,
    path: &str,
    peg: Option<u64>,
    start: Option<u64>,
    end: Option<u64>,
) -> Vec<Seg> {
    s.get_location_segments(
        &p(path),
        peg.map(Revnum),
        start.map(Revnum),
        end.map(Revnum),
        &CancelToken::new(),
    )
    .await
    .unwrap()
    .into_iter()
    .map(|s| {
        (
            s.path.map(|p| p.as_str().to_owned()),
            s.range_start.0,
            s.range_end.0,
        )
    })
    .collect()
}

fn seg(path: &str, start: u64, end: u64) -> Seg {
    (Some(path.to_owned()), start, end)
}

/// r1 trunk; r2 an edit; r3 a branch of trunk@1; r4 a branch of the branch (a file of it moved
/// in the same revision); r5 trunk deleted; r6 trunk added again from scratch.
fn history() -> FakeRaSession {
    let s = FakeRaSession::new("https://svn.test.local/repos/demo", "uuid-1").unwrap();
    s.seed("a", "r1", &[put("trunk/src/a.c", "1\n")]).unwrap();
    s.seed("a", "r2", &[put("trunk/src/a.c", "2\n")]).unwrap();
    s.seed("a", "r3", &[copy("trunk", 1, "branches/b")])
        .unwrap();
    s.seed(
        "a",
        "r4",
        &[
            copy("branches/b", 3, "branches/c"),
            copy("branches/b/src/a.c", 3, "branches/c/a.c"),
            FakeChange::Delete(p("branches/c/src/a.c")),
        ],
    )
    .unwrap();
    s.seed("a", "r5", &[FakeChange::Delete(p("trunk"))])
        .unwrap();
    s.seed("a", "r6", &[put("trunk/src/a.c", "new\n")]).unwrap();
    s
}

#[tokio::test]
async fn copies_continue_the_history_at_their_source_with_a_gap() {
    let s = history();
    assert_eq!(
        segments(&s, "branches/b/src/a.c", None, None, None).await,
        [
            seg("branches/b/src/a.c", 3, 6),
            (None, 2, 2),
            seg("trunk/src/a.c", 1, 1),
        ]
    );
    // A copy of a copy, and a file copied on its own in the same revision.
    assert_eq!(
        segments(&s, "branches/c/src", None, None, None).await,
        [
            seg("branches/c/src", 4, 6),
            seg("branches/b/src", 3, 3),
            (None, 2, 2),
            seg("trunk/src", 1, 1),
        ]
    );
    assert_eq!(
        segments(&s, "branches/c/a.c", None, None, None).await,
        [
            seg("branches/c/a.c", 4, 6),
            seg("branches/b/src/a.c", 3, 3),
            (None, 2, 2),
            seg("trunk/src/a.c", 1, 1),
        ]
    );
}

#[tokio::test]
async fn a_node_added_again_has_a_new_history() {
    let s = history();
    assert_eq!(
        segments(&s, "trunk/src/a.c", None, None, None).await,
        [seg("trunk/src/a.c", 6, 6)]
    );
    // At an older peg the old node is traced.
    assert_eq!(
        segments(&s, "trunk/src/a.c", Some(4), None, None).await,
        [seg("trunk/src/a.c", 1, 4)]
    );
    // Gone at the peg revision.
    let err = s
        .get_location_segments(
            &p("trunk"),
            Some(Revnum(5)),
            None,
            None,
            &CancelToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PathNotFound, "{err}");
}

#[tokio::test]
async fn the_range_crops_segments_and_the_root_starts_at_zero() {
    let s = history();
    assert_eq!(
        segments(&s, "branches/b/src/a.c", None, Some(4), Some(2)).await,
        [seg("branches/b/src/a.c", 3, 4), (None, 2, 2)]
    );
    assert_eq!(
        segments(&s, "branches/b/src/a.c", Some(6), Some(1), Some(1)).await,
        [seg("trunk/src/a.c", 1, 1)]
    );
    assert_eq!(segments(&s, "", None, None, None).await, [seg("", 0, 6)]);
    let err = s
        .get_location_segments(
            &p(""),
            Some(Revnum(3)),
            Some(Revnum(4)),
            None,
            &CancelToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.code,
        ErrorCode::Internal,
        "start younger than peg: {err}"
    );
}

#[tokio::test]
async fn the_path_is_taken_from_the_repository_root_whatever_the_session_url() {
    let s = history();
    let at_trunk = s
        .at(&RepoUrl::parse("https://svn.test.local/repos/demo/trunk").unwrap())
        .unwrap();
    assert_eq!(
        segments(&at_trunk, "branches/b", None, None, None).await,
        [seg("branches/b", 3, 6), (None, 2, 2), seg("trunk", 1, 1)]
    );
}

#[tokio::test]
async fn commit_editor_copies_keep_history_too() {
    use std::sync::Arc;

    use svn_ra_api::{CommitInfo, CommitObserver};

    struct Quiet;
    impl CommitObserver for Quiet {
        fn on_txn_created(&self, _: &str) -> svn_types::Result<()> {
            Ok(())
        }
        fn on_merged(&self, _: &CommitInfo) -> svn_types::Result<()> {
            Ok(())
        }
    }
    let s = history();
    let cancel = CancelToken::new();
    let mut ed = s
        .commit_editor("tag", &[], &[], false, Arc::new(Quiet), &cancel)
        .await
        .unwrap();
    let root = ed.open_root(None).await.unwrap();
    let tags = ed
        .add_directory(
            &p("tags"),
            root,
            Some(("https://svn.test.local/repos/demo/branches/b", Revnum(4))),
        )
        .await
        .unwrap();
    ed.close_directory(tags).await.unwrap();
    ed.close_directory(root).await.unwrap();
    ed.close_edit().await.unwrap();
    assert_eq!(
        segments(&s, "tags", None, None, None).await,
        [
            seg("tags", 7, 7),
            (None, 5, 6),
            seg("branches/b", 3, 4),
            (None, 2, 2),
            seg("trunk", 1, 1),
        ]
    );
}
