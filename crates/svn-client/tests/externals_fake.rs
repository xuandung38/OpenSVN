//! `svn:externals` edge cases over fake repositories: URL forms the oracle tests cannot share
//! with svn (absolute, `/`, `//`, another host), path safety, failures, nesting and
//! cancellation.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::Arc;

use common::*;
use svn_client::{ExternalAction, ExternalReport, UpdateSummary, checkout, update};
use svn_ra_api::fake::FakeRaSession;
use svn_types::{CancelToken, Depth, ErrorCode, RelPath, RepoUrl};

const OTHER_HOST: &str = "https://other.test.local/svn/x";

fn trunk(s: &FakeRaSession) -> FakeRaSession {
    s.at(&RepoUrl::parse(&format!("{ROOT_URL}/trunk")).unwrap())
        .unwrap()
}

/// `lib` and `trunk` in the main repository, `trunk/lib.txt` in another one on another host.
fn repos(defs: &str) -> (FakeRaSession, FakeRaSession) {
    let main = session();
    main.seed(
        "alice",
        "r1",
        &[
            put("lib/a.txt", "alpha\n"),
            put("lib/sub/s.txt", "sub\n"),
            put("trunk/sub/keep.txt", "keep\n"),
            prop("trunk", "svn:externals", Some(defs)),
        ],
    )
    .unwrap();
    let other = FakeRaSession::new(OTHER_HOST, "uuid-other").unwrap();
    other
        .seed("bob", "r1", &[put("trunk/lib.txt", "far away\n")])
        .unwrap();
    (main, other)
}

fn find<'a>(sum: &'a UpdateSummary, path: &str) -> &'a ExternalReport {
    sum.externals
        .iter()
        .find(|e| e.path.as_str() == path)
        .unwrap_or_else(|| panic!("no report for {path}: {:?}", sum.externals))
}

async fn co(
    main: &FakeRaSession,
    opener: Arc<FakeOpener>,
    sb: &Sandbox,
) -> (std::path::PathBuf, UpdateSummary) {
    let wc = sb.path("wc");
    let sum = checkout(
        &ext_ctx(opener),
        &trunk(main),
        &wc,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    (wc, sum)
}

#[tokio::test(flavor = "multi_thread")]
async fn absolute_server_and_scheme_relative_urls_reach_other_hosts_through_the_opener() {
    let (main, other) = repos(
        "https://other.test.local/svn/x/trunk abs\n\
         //other.test.local/svn/x/trunk scheme\n\
         /repos/demo/lib server\n\
         old-format https://other.test.local/svn/x/trunk\n",
    );
    let opener = FakeOpener::new(vec![main.clone(), other]);
    let sb = Sandbox::new();
    let (wc, sum) = co(&main, Arc::clone(&opener), &sb).await;
    for p in ["abs", "scheme", "server", "old-format"] {
        assert_eq!(find(&sum, p).action, ExternalAction::Fetched, "{p}");
    }
    assert_eq!(read(&wc.join("abs/lib.txt")), "far away\n");
    assert_eq!(read(&wc.join("scheme/lib.txt")), "far away\n");
    assert_eq!(read(&wc.join("server/a.txt")), "alpha\n");
    let opened = opener.opened.lock().unwrap().clone();
    assert!(
        opened
            .iter()
            .any(|u| u.starts_with("https://other.test.local/")),
        "the other host goes through the opener: {opened:?}"
    );
    // The other repository is recorded with its own root and UUID.
    let ext = open_db(&wc).external(&rp("abs")).unwrap().unwrap();
    assert_eq!(ext.repos_root, OTHER_HOST);
    assert_eq!(ext.repos_uuid, "uuid-other");
    assert_eq!(ext.def_repos_relpath, "trunk");
}

#[tokio::test(flavor = "multi_thread")]
async fn one_failing_external_does_not_stop_the_others() {
    let (main, other) = repos("^/missing gone\n^/lib ok\nsvn://h/r/x proto\n^/lib/sub/s.txt f\n");
    let sb = Sandbox::new();
    let (wc, sum) = co(&main, FakeOpener::new(vec![main.clone(), other]), &sb).await;
    let gone = find(&sum, "gone");
    assert_eq!(gone.action, ExternalAction::Failed);
    assert_eq!(gone.code, Some(ErrorCode::PathNotFound));
    assert_eq!(find(&sum, "proto").action, ExternalAction::Failed);
    assert_eq!(find(&sum, "ok").action, ExternalAction::Fetched);
    assert_eq!(find(&sum, "f").action, ExternalAction::Fetched);
    assert_eq!(read(&wc.join("ok/a.txt")), "alpha\n");
    assert!(!wc.join("gone").exists());
    assert_eq!(sum.external_warnings().count(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn definitions_that_leave_the_working_copy_are_refused() {
    for bad in [
        "^/lib ../escape",
        "^/lib /abs",
        "^/lib .svn/x",
        "^/lib sub/.SVN",
    ] {
        let (main, other) = repos(bad);
        let sb = Sandbox::new();
        let (wc, sum) = co(&main, FakeOpener::new(vec![main.clone(), other]), &sb).await;
        assert_eq!(sum.externals.len(), 1, "{bad}: {:?}", sum.externals);
        assert_eq!(sum.externals[0].action, ExternalAction::Failed, "{bad}");
        assert!(
            sum.externals[0].path.is_root(),
            "the whole definition fails"
        );
        assert!(!sb.dir.path().join("escape").exists());
        assert!(!wc.join(".svn/x").exists());
        assert!(
            open_db(&wc)
                .externals_under(&RelPath::root())
                .unwrap()
                .is_empty()
        );
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn an_external_is_never_written_through_a_symbolic_link() {
    let (main, other) = repos("^/lib sub/ext\n");
    let opener = FakeOpener::new(vec![main.clone(), other]);
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    checkout(
        &ctx(),
        &trunk(&main),
        &wc,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .unwrap();
    // A link in the way of the target, then a link as the target itself.
    let outside = sb.path("outside");
    std::fs::create_dir_all(&outside).unwrap();
    for (link, defs) in [("hop", "^/lib hop/ext\n"), ("ext2", "^/lib ext2\n")] {
        std::os::unix::fs::symlink(&outside, wc.join(link)).unwrap();
        let db = open_db(&wc);
        db.set_props(
            &RelPath::root(),
            &vec![(b"svn:externals".to_vec(), defs.as_bytes().to_vec())],
            &[],
        )
        .unwrap();
        drop(db);
        let sum = update(
            &ext_ctx(Arc::clone(&opener)),
            &trunk(&main),
            &wc,
            None,
            Depth::Unknown,
            &CancelToken::new(),
        )
        .await
        .unwrap();
        let r = &sum.externals[0];
        assert_eq!(
            r.action,
            ExternalAction::Failed,
            "{link}: {:?}",
            sum.externals
        );
        assert_eq!(r.code, Some(ErrorCode::UnsafePath), "{link}");
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0, "{link}");
        std::fs::remove_file(wc.join(link)).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn versioned_items_and_foreign_working_copies_in_the_way_are_left_alone() {
    let (main, other) = repos("^/lib sub\n^/lib foreign\n");
    let opener = FakeOpener::new(vec![main.clone(), other.clone()]);
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    checkout(
        &ctx(),
        &trunk(&main),
        &wc,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .unwrap();
    // A checkout of another repository where `foreign` goes, made by the user.
    checkout(
        &ctx(),
        &other,
        &wc.join("foreign"),
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .unwrap();
    let sum = update(
        &ext_ctx(opener),
        &trunk(&main),
        &wc,
        None,
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(find(&sum, "sub").action, ExternalAction::Failed);
    assert_eq!(find(&sum, "foreign").action, ExternalAction::Failed);
    assert_eq!(read(&wc.join("sub/keep.txt")), "keep\n");
    assert_eq!(read(&wc.join("foreign/trunk/lib.txt")), "far away\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_external_from_another_repository_is_refused() {
    let (main, other) = repos("https://other.test.local/svn/x/trunk/lib.txt far.txt\n");
    let sb = Sandbox::new();
    let (wc, sum) = co(&main, FakeOpener::new(vec![main.clone(), other]), &sb).await;
    let r = find(&sum, "far.txt");
    assert_eq!(r.action, ExternalAction::Failed);
    assert!(
        r.message.as_deref().unwrap().contains("not in repository"),
        "{r:?}"
    );
    assert!(!wc.join("far.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_definition_that_includes_itself_stops_at_the_nesting_limit() {
    let (main, other) = repos("^/trunk self\n");
    let sb = Sandbox::new();
    let (wc, sum) = co(&main, FakeOpener::new(vec![main.clone(), other]), &sb).await;
    let deepest = (0..svn_client::externals::MAX_NESTING)
        .map(|_| "self")
        .collect::<Vec<_>>()
        .join("/");
    assert!(wc.join(&deepest).join("sub/keep.txt").is_file());
    let limit = sum
        .externals
        .iter()
        .find(|e| e.code == Some(ErrorCode::LimitExceeded))
        .expect("limit reported");
    assert_eq!(limit.path.as_str(), deepest);
}

struct CancellingOpener {
    inner: Arc<FakeOpener>,
    cancel: CancelToken,
}

#[async_trait::async_trait]
impl svn_client::externals::SessionOpener for CancellingOpener {
    async fn open(
        &self,
        url: &RepoUrl,
        cancel: &CancelToken,
    ) -> svn_types::Result<Arc<dyn svn_ra_api::RaSession>> {
        self.cancel.cancel();
        self.inner.open(url, cancel).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_stops_processing_externals() {
    let (main, other) = repos("^/lib a\n^/lib b\n");
    let cancel = CancelToken::new();
    let mut c = ctx();
    c.externals = Some(Arc::new(CancellingOpener {
        inner: FakeOpener::new(vec![main.clone(), other]),
        cancel: cancel.clone(),
    }));
    let sb = Sandbox::new();
    let err = checkout(
        &c,
        &trunk(&main),
        &sb.path("wc"),
        None,
        Depth::Infinity,
        false,
        &cancel,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::Cancelled);
    assert!(!sb.path("wc/b").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_definition_changing_kind_replaces_the_old_external() {
    let (main, other) = repos("^/lib x\n^/lib/a.txt y\n");
    let opener = FakeOpener::new(vec![main.clone(), other]);
    let sb = Sandbox::new();
    let (wc, _) = co(&main, Arc::clone(&opener), &sb).await;
    assert!(wc.join("x/a.txt").is_file());
    assert!(wc.join("y").is_file());
    main.seed(
        "alice",
        "swap",
        &[prop(
            "trunk",
            "svn:externals",
            Some("^/lib/a.txt x\n^/lib y\n"),
        )],
    )
    .unwrap();
    let sum = update(
        &ext_ctx(opener),
        &trunk(&main),
        &wc,
        None,
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(sum.external_warnings().count(), 0, "{:?}", sum.externals);
    assert_eq!(read(&wc.join("x")), "alpha\n");
    assert!(wc.join("y/a.txt").is_file());
    let db = open_db(&wc);
    assert!(db.read_base(&rp("x")).unwrap().unwrap().file_external);
    assert_eq!(
        db.external(&rp("y")).unwrap().unwrap().kind,
        svn_types::NodeKind::Dir
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn externals_of_a_directory_deleted_on_the_server_go_with_it() {
    let main = session();
    main.seed(
        "alice",
        "r1",
        &[
            put("lib/a.txt", "alpha\n"),
            put("trunk/d/keep.txt", "keep\n"),
            prop(
                "trunk/d",
                "svn:externals",
                Some("^/lib ext\n^/lib/a.txt f\n"),
            ),
        ],
    )
    .unwrap();
    let opener = FakeOpener::new(vec![main.clone()]);
    let sb = Sandbox::new();
    let (wc, _) = co(&main, Arc::clone(&opener), &sb).await;
    assert!(wc.join("d/ext/a.txt").is_file());
    main.seed("alice", "r2", &[del("trunk/d")]).unwrap();
    let sum = update(
        &ext_ctx(opener),
        &trunk(&main),
        &wc,
        None,
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(find(&sum, "d/ext").action, ExternalAction::Removed);
    assert!(!wc.join("d").exists(), "{:?}", sum.externals);
    assert!(
        open_db(&wc)
            .externals_under(&RelPath::root())
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shallow_update_leaves_externals_alone() {
    let (main, other) = repos("^/lib ext\n");
    let opener = FakeOpener::new(vec![main.clone(), other]);
    let sb = Sandbox::new();
    let (wc, _) = co(&main, Arc::clone(&opener), &sb).await;
    main.seed("alice", "r2", &[prop("trunk", "svn:externals", None)])
        .unwrap();
    let sum = update(
        &ext_ctx(opener),
        &trunk(&main),
        &wc,
        None,
        Depth::Empty,
        &CancelToken::new(),
    )
    .await
    .unwrap();
    assert!(sum.externals.is_empty());
    assert!(wc.join("ext/a.txt").is_file());
}

#[tokio::test(flavor = "multi_thread")]
async fn updating_a_file_external_on_its_own_is_refused() {
    let (main, other) = repos("^/lib/a.txt f\n");
    let opener = FakeOpener::new(vec![main.clone(), other]);
    let sb = Sandbox::new();
    let (wc, _) = co(&main, Arc::clone(&opener), &sb).await;
    let err = update(
        &ext_ctx(opener),
        &trunk(&main),
        &wc.join("f"),
        None,
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::WcInvalidOperation);
    assert!(
        open_db(&wc)
            .read_base(&rp("f"))
            .unwrap()
            .unwrap()
            .file_external
    );
}
