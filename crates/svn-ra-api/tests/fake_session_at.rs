//! `FakeRaSession::at`: a session rooted below the repository root translates every path.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use bytes::Bytes;
use svn_ra_api::fake::{FakeChange, FakeRaSession};
use svn_ra_api::{CommitInfo, CommitObserver, LogRequest, RaSession, ReplayableSource};
use svn_types::{
    CancelToken, Checksum, Depth, ErrorCode, NodeKind, PropChange, RelPath, RepoUrl, Result, Revnum,
};

fn p(s: &str) -> RelPath {
    RelPath::new(s).unwrap()
}

fn url(s: &str) -> RepoUrl {
    RepoUrl::parse(s).unwrap()
}

const ROOT: &str = "https://svn.test.local/repos/demo";

fn repo() -> FakeRaSession {
    let s = FakeRaSession::new(ROOT, "uuid-1").unwrap();
    s.seed(
        "alice",
        "first",
        &[
            FakeChange::PutFile(p("trunk/a.txt"), Bytes::from_static(b"one")),
            FakeChange::PutFile(p("trunk/sub dir/b.txt"), Bytes::from_static(b"bee")),
            FakeChange::PutFile(p("README"), Bytes::from_static(b"top")),
        ],
    )
    .unwrap();
    s
}

struct NoObserver;
impl CommitObserver for NoObserver {
    fn on_txn_created(&self, _: &str) -> Result<()> {
        Ok(())
    }
    fn on_merged(&self, _: &CommitInfo) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn reads_are_relative_to_the_session_url() {
    let root = repo();
    let t = root.at(&url(&format!("{ROOT}/trunk"))).unwrap();
    let cancel = CancelToken::new();
    assert_eq!(t.session_url().as_url().as_str(), format!("{ROOT}/trunk"));
    assert_eq!(t.repos_root(), root.repos_root());
    assert_eq!(t.uuid(), root.uuid());
    assert_eq!(
        t.check_path(&p("a.txt"), None, &cancel).await.unwrap(),
        NodeKind::File
    );
    assert_eq!(
        t.check_path(&p("README"), None, &cancel).await.unwrap(),
        NodeKind::None
    );
    let names: Vec<String> = t
        .list(&RelPath::root(), None, Depth::Immediates, &cancel)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, vec!["a.txt", "sub dir"]);
    let mut out = Vec::new();
    t.get_file(&p("a.txt"), None, &mut out, &cancel)
        .await
        .unwrap();
    assert_eq!(out, b"one");

    // Percent-encoded segments are decoded, and the session shares the repository.
    let sub = root.at(&url(&format!("{ROOT}/trunk/sub%20dir"))).unwrap();
    assert!(
        sub.stat(&p("b.txt"), None, &cancel)
            .await
            .unwrap()
            .is_some()
    );

    let lock = t
        .lock(&p("a.txt"), None, false, None, &cancel)
        .await
        .unwrap();
    assert_eq!(lock.path.as_str(), "/trunk/a.txt");
    assert_eq!(
        root.get_locks(&p("trunk"), Depth::Infinity, &cancel)
            .await
            .unwrap()
            .len(),
        1
    );
    t.unlock(&p("a.txt"), Some(&lock.token), false, &cancel)
        .await
        .unwrap();

    // An empty path list means the session URL.
    let mut revs = Vec::new();
    root.seed(
        "bob",
        "readme",
        &[FakeChange::PutFile(p("README"), Bytes::from_static(b"x"))],
    )
    .unwrap();
    let req = LogRequest {
        paths: vec![],
        start: None,
        end: None,
        limit: None,
        discover_changed_paths: false,
        strict_node_history: false,
    };
    t.log(
        &req,
        &mut |e| {
            revs.push(e.rev);
            Ok(())
        },
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(revs, vec![Revnum(1)]);
}

#[tokio::test]
async fn sessions_outside_the_repository_are_refused() {
    let root = repo();
    for u in [
        "https://svn.test.local/repos",
        "https://svn.test.local/repos/demo2/trunk",
        "https://other.test.local/repos/demo/trunk",
        "http://svn.test.local/repos/demo/trunk",
    ] {
        let err = root.at(&url(u)).err().expect(u);
        assert_eq!(err.code, ErrorCode::UnsafePath, "{u}");
    }
}

#[tokio::test]
async fn commits_are_relative_to_the_session_url() {
    let root = repo();
    let t = root.at(&url(&format!("{ROOT}/trunk"))).unwrap();
    let cancel = CancelToken::new();
    let lock = t
        .lock(&p("a.txt"), None, false, None, &cancel)
        .await
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("c");
    std::fs::write(&src, "sea").unwrap();
    let mut ed = t
        .commit_editor(
            "add c",
            &[],
            &[("a.txt".to_owned(), lock.token.clone())],
            false,
            Arc::new(NoObserver),
            &cancel,
        )
        .await
        .unwrap();
    let dir = ed.open_root(Some(Revnum(1))).await.unwrap();
    ed.change_dir_prop(
        dir,
        PropChange {
            name: "p".into(),
            value: Some("v".into()),
        },
    )
    .await
    .unwrap();
    let c = ed.add_file(&p("c.txt"), dir, None).await.unwrap();
    ed.apply_text(
        c,
        None,
        ReplayableSource {
            path: src,
            translated: true,
            size: Some(3),
        },
        None,
        Checksum::Md5([0; 16]),
    )
    .await
    .unwrap();
    ed.close_file(c).await.unwrap();
    ed.delete_entry(&p("a.txt"), Some(Revnum(1)), dir)
        .await
        .unwrap();
    ed.close_directory(dir).await.unwrap();
    let info = ed.close_edit().await.unwrap();
    assert_eq!(info.revision, Revnum(2));

    let mut out = Vec::new();
    root.get_file(&p("trunk/c.txt"), None, &mut out, &cancel)
        .await
        .unwrap();
    assert_eq!(out, b"sea");
    assert_eq!(
        root.check_path(&p("trunk/a.txt"), None, &cancel)
            .await
            .unwrap(),
        NodeKind::None
    );
    assert_eq!(
        root.check_path(&p("README"), None, &cancel).await.unwrap(),
        NodeKind::File
    );
    let props = root
        .stat(&p("trunk"), None, &cancel)
        .await
        .unwrap()
        .unwrap();
    assert!(props.has_props, "the property went to trunk, not the root");
    assert!(
        root.get_locks(&RelPath::root(), Depth::Infinity, &cancel)
            .await
            .unwrap()
            .is_empty()
    );
}
