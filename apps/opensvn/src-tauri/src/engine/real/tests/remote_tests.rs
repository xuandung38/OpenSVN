//! Repository reads, locks, blame, downloads and the shared engine suite.

use std::path::{Path, PathBuf};

use opensvn_ipc::{EntryKind, Layout, LogGetRequest, LogTarget, Rev, WcId};
use svn_ra_api::fake::FakeChange;
use svn_types::{ErrorCode, RelPath, SvnError};

use super::support::{
    URL, add_base_dir, add_base_file, count_rows, ctx, dir, fixture, make_wc, plant_dead_app_lock,
    plant_lock_row, put,
};
use crate::engine::conformance::{self, Harness, PlantedOwner};
use crate::engine::real::RealEngine;
use crate::engine::{
    DiffOptions, EngineFacade, LockScope, LogScope, OpContext, RepoLocation, WcRef,
};

/// Harness for the repository-only groups of the shared suite on the real engine (no working
/// copy writes). The whole suite, working copy groups included, runs in `conformance_tests`.
struct RealHarness {
    fixture: super::support::Fixture,
    scratch: tempfile::TempDir,
}

impl Harness for RealHarness {
    fn engine(&self) -> &dyn EngineFacade {
        &self.fixture.engine
    }
    fn repo_url(&self) -> String {
        URL.to_owned()
    }
    fn work_dir(&self) -> PathBuf {
        self.scratch.path().join("work")
    }
    fn write_file(&self, _root: &Path, _rel: &str, _content: &str) -> Result<(), SvnError> {
        Err(SvnError::new(ErrorCode::Internal, "no working copy writes"))
    }
    fn remove_file(&self, _root: &Path, _rel: &str) -> Result<(), SvnError> {
        Err(SvnError::new(ErrorCode::Internal, "no working copy writes"))
    }
    fn scratch_dir(&self) -> PathBuf {
        self.scratch.path().to_path_buf()
    }
    fn plant_lock(&self, root: &Path, dir: &str, owner: PlantedOwner) -> Result<(), SvnError> {
        match owner {
            PlantedOwner::DeadProcess => plant_dead_app_lock(root, &self.fixture.store, dir),
            PlantedOwner::Foreign => plant_lock_row(root, dir),
        }
        Ok(())
    }
}

fn harness() -> RealHarness {
    RealHarness {
        fixture: fixture(),
        scratch: tempfile::tempdir().unwrap(),
    }
}

#[tokio::test]
async fn shared_suite_repository_reads() {
    conformance::repository_reads(&harness()).await;
}

#[tokio::test]
async fn shared_suite_repository_content_tools() {
    conformance::repository_content_tools(&harness()).await;
}

#[tokio::test]
async fn shared_suite_cancellation_and_errors() {
    conformance::cancellation_and_errors(&harness()).await;
}

/// The recovery half of `open_recovers_dead_locks_and_reports_foreign_ones`, on a working copy
/// made by `svn-wc` instead of a checkout, with locks planted through the harness.
#[tokio::test]
async fn shared_recovery_scenario_with_planted_locks() {
    let h = harness();
    let e = h.engine();
    let (_tmp, root) = make_wc(1);
    add_base_dir(&root, "src", "trunk/src", 1);
    let clean = e.open_wc(&root, &ctx()).await.unwrap();
    assert!(clean.recovery.recovered.is_empty() && clean.recovery.needs_user_action.is_empty());

    h.plant_lock(&root, "src", PlantedOwner::DeadProcess)
        .unwrap();
    let opened = e.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.recovered, vec!["src".to_owned()]);
    assert!(opened.recovery.needs_user_action.is_empty());
    let again = e.open_wc(&root, &ctx()).await.unwrap();
    assert!(again.recovery.recovered.is_empty(), "recovery happens once");

    h.plant_lock(&root, "src", PlantedOwner::Foreign).unwrap();
    for _ in 0..2 {
        let opened = e.open_wc(&root, &ctx()).await.unwrap();
        assert!(opened.recovery.recovered.is_empty());
        assert_eq!(opened.recovery.needs_user_action, vec!["src".to_owned()]);
    }
}

#[tokio::test]
async fn info_and_list_report_authors_dates_and_directories_first() {
    let f = fixture();
    let info = f.engine.repo_info(URL, &ctx()).await.unwrap();
    assert_eq!(info.repo_root_url, URL);
    assert_eq!(info.repo_uuid, "real-uuid");
    assert_eq!(info.youngest_rev, Rev(1));
    let file = f
        .engine
        .repo_info(&format!("{URL}/trunk/README.md"), &ctx())
        .await
        .unwrap();
    assert_eq!(file.kind, EntryKind::File);

    let entries = f
        .engine
        .repo_list(&format!("{URL}/trunk"), Some(Rev(1)), &ctx())
        .await
        .unwrap()
        .entries;
    assert_eq!(entries[0].name, "src");
    assert_eq!(entries[0].kind, EntryKind::Dir);
    let readme = entries.iter().find(|e| e.name == "README.md").unwrap();
    assert_eq!(readme.created_rev, Rev(1));
    // Author and date come from the server's directory entry; the in-memory repository has none.
    assert_eq!(readme.size.map(|s| s.0), Some(6));

    let missing = f
        .engine
        .repo_info(&format!("{URL}/trunk/none"), &ctx())
        .await
        .unwrap_err();
    assert_eq!(missing.code, ErrorCode::PathNotFound);
    let foreign = f
        .engine
        .repo_info("https://elsewhere.test/repo", &ctx())
        .await
        .unwrap_err();
    assert_eq!(foreign.code, ErrorCode::Network);
}

#[tokio::test]
async fn encoded_urls_address_paths_with_spaces() {
    let f = fixture();
    f.repo
        .seed("bob", "Spaced name", &[put("trunk/a b.txt", "spaced\n")])
        .unwrap();
    let text = f
        .engine
        .repo_cat(&format!("{URL}/trunk/a%20b.txt"), None, &ctx())
        .await
        .unwrap();
    assert_eq!(text.content, "spaced\n");
}

#[tokio::test]
async fn cat_refuses_files_above_the_inline_limit() {
    let f = fixture();
    let big = "x".repeat(super::super::remote::CAT_LIMIT + 1);
    f.repo
        .seed("bob", "Big file", &[put("trunk/big.txt", &big)])
        .unwrap();
    let err = f
        .engine
        .repo_cat(&format!("{URL}/trunk/big.txt"), None, &ctx())
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Internal);
    assert!(err.message.contains("too large"));
}

#[tokio::test]
async fn cancelled_calls_stop_before_touching_the_repository() {
    let f = fixture();
    let cancelled = OpContext::detached();
    cancelled.cancel.cancel();
    assert_eq!(
        f.engine.repo_info(URL, &cancelled).await.unwrap_err().code,
        ErrorCode::Cancelled
    );
    assert_eq!(
        f.engine
            .repo_list(&format!("{URL}/trunk"), None, &cancelled)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Cancelled
    );
}

fn log_request(limit: u32, filter: Option<&str>) -> LogGetRequest {
    LogGetRequest {
        target: LogTarget::Url {
            url: format!("{URL}/trunk"),
        },
        from_rev: None,
        to_rev: None,
        limit,
        stop_on_copy: None,
        with_paths: false,
        filter: filter.map(str::to_owned),
    }
}

fn seed_history(f: &super::support::Fixture) {
    for (author, msg, body) in [
        ("bob", "Fix typo", "hello world\n"),
        ("carol", "Add feature", "hello world\nfeature\n"),
        ("bob", "Tidy up", "hello world\nfeature\ntidy\n"),
    ] {
        f.repo
            .seed(author, msg, &[put("trunk/README.md", body)])
            .unwrap();
    }
}

#[tokio::test]
async fn log_pages_are_newest_first_and_link_to_the_next_page() {
    let f = fixture();
    seed_history(&f);
    let scope = LogScope::Url(format!("{URL}/trunk"));
    let first = f
        .engine
        .log(&scope, &log_request(2, None), &ctx())
        .await
        .unwrap();
    let revs: Vec<u64> = first.entries.iter().map(|e| e.rev.0).collect();
    assert_eq!(revs, vec![4, 3]);
    assert_eq!(first.next_rev, Some(Rev(2)));

    let mut next = log_request(2, None);
    next.from_rev = first.next_rev;
    let second = f.engine.log(&scope, &next, &ctx()).await.unwrap();
    let revs: Vec<u64> = second.entries.iter().map(|e| e.rev.0).collect();
    assert_eq!(revs, vec![2, 1]);
    assert_eq!(second.next_rev, None);
}

#[tokio::test]
async fn log_filter_matches_author_and_message_case_insensitively() {
    let f = fixture();
    seed_history(&f);
    let scope = LogScope::Url(format!("{URL}/trunk"));
    let by_author = f
        .engine
        .log(&scope, &log_request(10, Some("BOB")), &ctx())
        .await
        .unwrap();
    assert_eq!(
        by_author
            .entries
            .iter()
            .map(|e| e.rev.0)
            .collect::<Vec<_>>(),
        vec![4, 2]
    );
    let by_message = f
        .engine
        .log(&scope, &log_request(10, Some("feature")), &ctx())
        .await
        .unwrap();
    assert_eq!(by_message.entries.len(), 1);
    assert_eq!(by_message.entries[0].author.as_deref(), Some("carol"));
}

#[tokio::test]
async fn blame_attributes_each_line_to_the_revision_that_introduced_it() {
    let f = fixture();
    seed_history(&f);
    let lines = f
        .engine
        .blame(&format!("{URL}/trunk/README.md"), None, &ctx())
        .await
        .unwrap();
    let got: Vec<(u64, &str, &str)> = lines
        .iter()
        .map(|l| (l.rev.0, l.author.as_deref().unwrap_or(""), l.text.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![
            (2, "bob", "hello world"),
            (3, "carol", "feature"),
            (4, "bob", "tidy"),
        ]
    );
    let at_rev3 = f
        .engine
        .blame(&format!("{URL}/trunk/README.md"), Some(Rev(3)), &ctx())
        .await
        .unwrap();
    assert_eq!(at_rev3.len(), 2);
    assert_eq!(at_rev3[1].rev, Rev(3));
}

#[tokio::test]
async fn blame_rejects_binary_files_and_missing_paths() {
    let f = fixture();
    f.repo
        .seed(
            "bob",
            "Binary",
            &[FakeChange::PutFile(
                RelPath::new("trunk/blob.bin").unwrap(),
                bytes::Bytes::from_static(&[0, 159, 146, 150]),
            )],
        )
        .unwrap();
    let err = f
        .engine
        .blame(&format!("{URL}/trunk/blob.bin"), None, &ctx())
        .await
        .unwrap_err();
    assert!(err.message.contains("binary"), "{}", err.message);
    let missing = f
        .engine
        .blame(&format!("{URL}/trunk/none.txt"), None, &ctx())
        .await
        .unwrap_err();
    assert_eq!(missing.code, ErrorCode::PathNotFound);
}

#[tokio::test]
async fn repo_save_writes_a_complete_file_or_nothing() {
    let f = fixture();
    let out = tempfile::tempdir().unwrap();
    let target = out.path().join("README.md");
    f.engine
        .repo_save(&format!("{URL}/trunk/README.md"), None, &target, &ctx())
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello\n");

    let relative = f
        .engine
        .repo_save(
            &format!("{URL}/trunk/README.md"),
            None,
            Path::new("rel.txt"),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert_eq!(relative.code, ErrorCode::Internal);

    let missing = out.path().join("missing.txt");
    let err = f
        .engine
        .repo_save(&format!("{URL}/trunk/none"), None, &missing, &ctx())
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PathNotFound);
    assert!(!missing.exists());

    let cancelled = OpContext::detached();
    cancelled.cancel.cancel();
    let other = out.path().join("cancelled.txt");
    let err = f
        .engine
        .repo_save(&format!("{URL}/trunk/README.md"), None, &other, &cancelled)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Cancelled);
    let leftovers: Vec<_> = std::fs::read_dir(out.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(leftovers, vec!["README.md".to_owned()], "no partial files");
}

#[tokio::test]
async fn custom_layout_expands_projects() {
    let f = fixture();
    f.repo
        .seed(
            "bob",
            "Project layout",
            &[
                dir("proj"),
                dir("proj/trunk"),
                dir("proj/branches"),
                dir("proj/branches/feature-x"),
                dir("proj/tags"),
            ],
        )
        .unwrap();
    let layout = Layout::Custom {
        trunk: "/{project}/trunk".into(),
        branches: vec!["/{project}/branches".into()],
        tags: vec!["/{project}/tags".into()],
    };
    let nodes = f.engine.branches_list(URL, &layout, &ctx()).await.unwrap();
    assert!(nodes.iter().any(|n| n.url == format!("{URL}/proj/trunk")));
    assert!(nodes.iter().all(|n| !n.url.contains("{project}")));
    assert!(
        nodes
            .iter()
            .any(|n| n.name == "feature-x" && n.url == format!("{URL}/proj/branches/feature-x"))
    );
}

#[tokio::test]
async fn url_locks_are_visible_in_listings_until_released() {
    let f = fixture();
    let file = format!("{URL}/trunk/README.md");
    let scope = LockScope::Urls(vec![file.clone()]);
    f.engine
        .lock(&scope, Some("editing"), false, &ctx())
        .await
        .unwrap();
    let list = f
        .engine
        .repo_list(&format!("{URL}/trunk"), None, &ctx())
        .await
        .unwrap()
        .entries;
    let lock = list
        .iter()
        .find(|e| e.name == "README.md")
        .and_then(|e| e.lock.clone())
        .expect("lock in listing");
    assert_eq!(lock.comment.as_deref(), Some("editing"));
    assert!(!lock.mine && lock.token.is_none());

    let again = f
        .engine
        .lock(&scope, None, false, &ctx())
        .await
        .unwrap_err();
    assert_eq!(again.code, ErrorCode::LockedByOther);
    f.engine
        .lock(&scope, None, true, &ctx())
        .await
        .expect("steal");

    f.engine.unlock(&scope, true, &ctx()).await.unwrap();
    let list = f
        .engine
        .repo_list(&format!("{URL}/trunk"), None, &ctx())
        .await
        .unwrap()
        .entries;
    assert!(
        list.iter()
            .find(|e| e.name == "README.md")
            .unwrap()
            .lock
            .is_none()
    );
}

/// mod_dav_svn listings carry no locks: a HEAD listing asks for them separately, an older
/// revision's listing shows none (locks only exist in HEAD).
#[tokio::test]
async fn dav_listings_get_their_locks_from_get_locks() {
    use std::sync::Arc;

    use super::support::{StaticConnector, fixture_on, standard_repo};
    use crate::engine::real::ClientRecovery;

    let repo = standard_repo().without_listed_locks();
    let f = fixture_on(
        repo.clone(),
        Arc::new(StaticConnector(vec![repo])),
        Arc::new(ClientRecovery),
    );
    let scope = LockScope::Urls(vec![format!("{URL}/trunk/README.md")]);
    f.engine
        .lock(&scope, Some("editing"), false, &ctx())
        .await
        .unwrap();
    let lock_of = |list: &[opensvn_ipc::RepoEntry]| {
        list.iter()
            .find(|e| e.name == "README.md")
            .and_then(|e| e.lock.clone())
    };
    let head = f
        .engine
        .repo_list(&format!("{URL}/trunk"), None, &ctx())
        .await
        .unwrap()
        .entries;
    let lock = lock_of(&head).expect("lock in a HEAD listing");
    assert_eq!(lock.comment.as_deref(), Some("editing"));
    let root = f.engine.repo_list(URL, None, &ctx()).await.unwrap().entries;
    assert!(
        root.iter().all(|e| e.lock.is_none()),
        "a lock below a child is not the child's: {root:?}"
    );
    let old = f
        .engine
        .repo_list(&format!("{URL}/trunk"), Some(Rev(1)), &ctx())
        .await
        .unwrap()
        .entries;
    assert!(lock_of(&old).is_none(), "{old:?}");
}

fn wc_ref(root: &Path) -> WcRef {
    WcRef {
        id: WcId("wc".into()),
        root: root.to_path_buf(),
    }
}

#[tokio::test]
async fn working_copy_locks_are_recorded_in_wc_db_and_cleared_on_unlock() {
    let f = fixture();
    let (_tmp, root) = make_wc(1);
    add_base_file(&root, "README.md", "trunk/README.md", 1);
    let scope = LockScope::Wc {
        wc: wc_ref(&root),
        paths: vec!["README.md".into()],
    };
    f.engine
        .lock(&scope, Some("mine"), false, &ctx())
        .await
        .unwrap();
    assert_eq!(count_rows(&root, "lock"), 1);
    let on_server = f
        .engine
        .repo_list(&format!("{URL}/trunk"), None, &ctx())
        .await
        .unwrap()
        .entries;
    assert!(
        on_server
            .iter()
            .find(|e| e.name == "README.md")
            .unwrap()
            .lock
            .is_some()
    );

    assert_eq!(count_rows(&root, "wc_lock"), 0, "directory lock released");

    f.engine.unlock(&scope, false, &ctx()).await.unwrap();
    assert_eq!(count_rows(&root, "lock"), 0);
    assert_eq!(count_rows(&root, "wc_lock"), 0, "directory lock released");
    let on_server = f
        .engine
        .repo_list(&format!("{URL}/trunk"), None, &ctx())
        .await
        .unwrap()
        .entries;
    assert!(
        on_server
            .iter()
            .find(|e| e.name == "README.md")
            .unwrap()
            .lock
            .is_none()
    );
}

#[tokio::test]
async fn working_copy_locks_wait_for_a_directory_locked_by_another_program() {
    let f = fixture();
    let (_tmp, root) = make_wc(1);
    add_base_file(&root, "README.md", "trunk/README.md", 1);
    plant_lock_row(&root, "");
    let scope = LockScope::Wc {
        wc: wc_ref(&root),
        paths: vec!["README.md".into()],
    };
    let err = f
        .engine
        .lock(&scope, None, false, &ctx())
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::WcBusy);
    assert_eq!(count_rows(&root, "lock"), 0);
    assert_eq!(
        count_rows(&root, "wc_lock"),
        1,
        "the other program's lock stays"
    );
    let on_server = f
        .engine
        .repo_list(&format!("{URL}/trunk"), None, &ctx())
        .await
        .unwrap()
        .entries;
    let readme = on_server.iter().find(|e| e.name == "README.md").unwrap();
    assert!(readme.lock.is_none(), "the server was never asked");
}

#[tokio::test]
async fn locking_a_path_that_is_not_in_the_repository_fails_before_any_server_call() {
    let f = fixture();
    let (_tmp, root) = make_wc(1);
    let scope = LockScope::Wc {
        wc: wc_ref(&root),
        paths: vec!["new-file.txt".into()],
    };
    let err = f
        .engine
        .lock(&scope, None, false, &ctx())
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PathNotFound);
    assert_eq!(count_rows(&root, "lock"), 0);
}

#[tokio::test]
async fn working_copy_log_follows_the_repository_path_of_the_node() {
    let f = fixture();
    seed_history(&f);
    let (_tmp, root) = make_wc(4);
    add_base_file(&root, "README.md", "trunk/README.md", 4);
    let loc = f
        .engine
        .node_location(&wc_ref(&root), "README.md", &ctx())
        .await
        .unwrap();
    assert_eq!(
        loc,
        RepoLocation {
            repo_root: URL.to_owned(),
            repos_path: "trunk/README.md".into(),
        }
    );
    let page = f
        .engine
        .log(&LogScope::Repo(loc), &log_request(10, None), &ctx())
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 4);
}

/// A switched node logs its own repository path, not the working copy URL plus the path.
#[tokio::test]
async fn working_copy_log_of_a_switched_node_reads_the_branch() {
    let f = fixture();
    seed_history(&f);
    f.repo
        .seed(
            "dave",
            "Branch work",
            &[
                dir("branches"),
                dir("branches/x"),
                put("branches/x/README.md", "branch\n"),
            ],
        )
        .unwrap();
    let (_tmp, root) = make_wc(5);
    add_base_file(&root, "README.md", "branches/x/README.md", 5);
    let loc = f
        .engine
        .node_location(&wc_ref(&root), "README.md", &ctx())
        .await
        .unwrap();
    assert_eq!(loc.repos_path, "branches/x/README.md");
    let page = f
        .engine
        .log(&LogScope::Repo(loc), &log_request(10, None), &ctx())
        .await
        .unwrap();
    let messages: Vec<&str> = page.entries.iter().map(|e| e.message.as_str()).collect();
    assert_eq!(messages, ["Branch work"]);

    let err = f
        .engine
        .node_location(&wc_ref(&root), "missing.txt", &ctx())
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PathNotFound);
}

#[allow(dead_code)]
fn assert_engine_is_object_safe(e: &RealEngine) -> &dyn EngineFacade {
    e
}

#[tokio::test]
async fn branch_urls_are_percent_encoded_and_unsafe_names_skipped() {
    let f = fixture();
    f.repo
        .seed(
            "bob",
            "Odd branch names",
            &[
                dir("branches/feature x"),
                dir("branches/50%off"),
                dir("branches/a#b?c"),
                dir("branches/hash#1"),
                dir("branches/CON"),
                dir("tags/v1.0"),
            ],
        )
        .unwrap();
    let nodes = f
        .engine
        .branches_list(URL, &Layout::Auto, &ctx())
        .await
        .unwrap();
    let url_of = |name: &str| nodes.iter().find(|n| n.name == name).map(|n| n.url.clone());
    assert_eq!(url_of("trunk"), Some(format!("{URL}/trunk")));
    assert_eq!(
        url_of("feature x"),
        Some(format!("{URL}/branches/feature%20x"))
    );
    assert_eq!(url_of("50%off"), Some(format!("{URL}/branches/50%25off")));
    assert_eq!(url_of("hash#1"), Some(format!("{URL}/branches/hash%231")));
    assert_eq!(url_of("v1.0"), Some(format!("{URL}/tags/v1.0")));
    // `?` is not a valid Windows file name character: a branch named like that cannot be
    // checked out safely, so it is not offered (same for device names such as CON).
    assert_eq!(url_of("a#b?c"), None);
    assert_eq!(url_of("CON"), None);
    // Every listed URL resolves back to the directory it names.
    for n in nodes.iter().filter(|n| n.name != "trunk") {
        let info = f.engine.repo_info(&n.url, &ctx()).await.unwrap();
        assert_eq!(info.kind, EntryKind::Dir, "{}", n.url);
    }
}

/// Drives `op` until the working copy write lock is taken, then drops it mid-operation.
async fn drop_while_the_directory_is_locked<F: std::future::Future>(root: &Path, op: F) {
    let mut op = Box::pin(op);
    for _ in 0..500 {
        tokio::select! {
            biased;
            _ = &mut op => panic!("the operation was expected to hang"),
            () = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
        }
        if count_rows(root, "wc_lock") == 1 {
            drop(op);
            // Dropped while the lock was still being taken, the guard is dropped by the database
            // thread as soon as that finishes: allow it a moment.
            for _ in 0..500 {
                if count_rows(root, "wc_lock") == 0 {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("the directory lock outlived the dropped operation");
        }
    }
    panic!("the directory lock was never taken");
}

#[tokio::test]
async fn a_dropped_working_copy_lock_or_unlock_releases_the_directory_lock() {
    use std::sync::Arc;

    use super::support::{ScriptedConnector, ScriptedSession, fixture_on, standard_repo};
    let repo = standard_repo();
    let session = Arc::new(ScriptedSession::new(repo.clone(), None));
    let f = fixture_on(
        repo,
        Arc::new(ScriptedConnector(session.clone())),
        Arc::new(crate::engine::real::ClientRecovery),
    );
    let (_tmp, root) = make_wc(1);
    add_base_file(&root, "README.md", "trunk/README.md", 1);
    let scope = LockScope::Wc {
        wc: wc_ref(&root),
        paths: vec!["README.md".into()],
    };
    session
        .hang_locks
        .store(true, std::sync::atomic::Ordering::SeqCst);

    drop_while_the_directory_is_locked(&root, f.engine.lock(&scope, None, false, &ctx())).await;
    assert_eq!(
        count_rows(&root, "wc_lock"),
        0,
        "released by the lock guard"
    );
    assert_eq!(count_rows(&root, "lock"), 0);

    drop_while_the_directory_is_locked(&root, f.engine.unlock(&scope, false, &ctx())).await;
    assert_eq!(
        count_rows(&root, "wc_lock"),
        0,
        "released by the unlock guard"
    );

    // The directory is usable again: a normal lock and unlock go through.
    session
        .hang_locks
        .store(false, std::sync::atomic::Ordering::SeqCst);
    f.engine.lock(&scope, None, false, &ctx()).await.unwrap();
    assert_eq!(count_rows(&root, "lock"), 1);
    f.engine.unlock(&scope, false, &ctx()).await.unwrap();
    assert_eq!(count_rows(&root, "wc_lock"), 0);
}

/// Revision diffs apply the same `svn_diff` display budget as the fake engine: over it the
/// sides are dropped and `too_complex` is set, unless `full_text` asks for them (external tool).
#[tokio::test]
async fn revision_diff_over_the_display_budget_is_flagged_too_complex() {
    let f = fixture();
    let numbered = |rev: bool| {
        let mut v: Vec<usize> = (0..20_000).collect();
        if rev {
            v.reverse();
        }
        v.iter().map(|i| format!("line {i}\n")).collect::<String>()
    };
    f.repo
        .seed("bob", "Add", &[put("trunk/big.txt", &numbered(false))])
        .unwrap();
    f.repo
        .seed("bob", "Reverse", &[put("trunk/big.txt", &numbered(true))])
        .unwrap();
    let url = format!("{URL}/trunk/big.txt");
    let d = f
        .engine
        .diff_rev(&url, None, Rev(3), DiffOptions::default(), &ctx())
        .await
        .unwrap();
    assert_eq!(d.too_complex, Some(true));
    assert!(d.left.is_none() && d.right.is_none() && !d.binary);

    let full = DiffOptions {
        full_text: true,
        ..DiffOptions::default()
    };
    let d = f
        .engine
        .diff_rev(&url, None, Rev(3), full, &ctx())
        .await
        .unwrap();
    assert_eq!(d.too_complex, None);
    assert!(d.left.unwrap().content.starts_with("line 0\n"));
    assert!(d.right.unwrap().content.starts_with("line 19999\n"));

    // A small edit keeps both sides.
    f.repo
        .seed("bob", "Small", &[put("trunk/README.md", "hello world\n")])
        .unwrap();
    let d = f
        .engine
        .diff_rev(
            &format!("{URL}/trunk/README.md"),
            None,
            Rev(4),
            DiffOptions::default(),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(d.too_complex, None);
    assert_eq!(d.left.unwrap().content, "hello\n");
}

/// Entries the session refuses (unsafe names) stay out of the listing, but their number reaches
/// Browse so it does not show the folder as empty.
#[tokio::test]
async fn a_listing_reports_how_many_entries_were_refused() {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use super::support::{ScriptedConnector, ScriptedSession, fixture_on, standard_repo};
    let repo = standard_repo();
    let session = Arc::new(ScriptedSession::new(repo.clone(), None));
    let f = fixture_on(
        repo,
        Arc::new(ScriptedConnector(session.clone())),
        Arc::new(crate::engine::real::ClientRecovery),
    );
    let plain = f
        .engine
        .repo_list(&format!("{URL}/trunk"), None, &ctx())
        .await
        .unwrap();
    assert_eq!(plain.hidden, 0);

    session.refused_in_listings.store(3, Ordering::SeqCst);
    let listing = f
        .engine
        .repo_list(&format!("{URL}/trunk"), None, &ctx())
        .await
        .unwrap();
    assert_eq!(listing.hidden, 3);
    assert_eq!(listing.entries, plain.entries);
}
