//! Commit through `svn-client`: the shared commit scenarios, auto add/remove (incomplete folders,
//! conflict markers), a lost `MERGE` answer settled on the spot or later by recovery, and journals
//! found again by recovery whatever path the working copy is opened under.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use opensvn_ipc::{
    CommitRequest, CopyLocationRequest, Depth, NodeStatus, ResolveChoice, ResolveRequest, Rev,
    UpdateRequest, WcFormat, WcId,
};
use svn_client::commit::journal::JournalStore;
use svn_ra_api::RaSession;
use svn_ra_api::fake::{FakeMergeFault, FakeRaSession};
use svn_types::{CancelToken, ErrorCode, NodeKind, RelPath, SvnError};
use svn_wc::WcDb;

use super::support::{
    AnchoredConnector, Fixture, URL, ctx, fixture_on, plant_dead_app_lock, plant_lock_row, put,
    standard_repo,
};
use crate::engine::conformance::{self, Harness, PlantedOwner};
use crate::engine::real::ClientRecovery;
use crate::engine::{CheckoutPlan, EngineFacade, StatusQuery, WcRef};

struct Env {
    f: Fixture,
    connector: Arc<AnchoredConnector>,
    tmp: tempfile::TempDir,
}

fn env() -> Env {
    let repo = standard_repo();
    let connector = Arc::new(AnchoredConnector::new(repo.clone()));
    Env {
        f: fixture_on(repo, connector.clone(), Arc::new(ClientRecovery)),
        connector,
        tmp: tempfile::tempdir().unwrap(),
    }
}

impl Env {
    fn repo(&self) -> &FakeRaSession {
        &self.f.repo
    }

    async fn checkout(&self, name: &str) -> WcRef {
        let root = self.tmp.path().join(name);
        let info = self
            .f
            .engine
            .checkout(
                &CheckoutPlan {
                    url: format!("{URL}/trunk"),
                    path: root,
                    rev: None,
                    depth: Depth::Infinity,
                    sparse: None,
                    format: WcFormat::V31,
                    store_pristine: true,
                    ignore_externals: false,
                },
                &ctx(),
            )
            .await
            .expect("checkout");
        WcRef {
            id: WcId(name.to_owned()),
            root: PathBuf::from(info.root_path),
        }
    }

    async fn commit(&self, wc: &WcRef, paths: &[&str], auto: bool) -> Result<Rev, SvnError> {
        self.f
            .engine
            .commit(wc, &request(wc, paths, auto), &ctx())
            .await
    }

    async fn status(&self, wc: &WcRef) -> Vec<opensvn_ipc::FileStatus> {
        self.f
            .engine
            .status(
                wc,
                StatusQuery {
                    include_ignored: false,
                    include_unversioned: true,
                },
                &ctx(),
            )
            .await
            .expect("status")
    }

    async fn text_of(&self, wc: &WcRef, path: &str) -> Option<NodeStatus> {
        self.status(wc)
            .await
            .into_iter()
            .find(|s| s.path == path)
            .map(|s| s.text)
    }

    async fn kind_in_repo(&self, path: &str) -> NodeKind {
        self.repo()
            .check_path(&RelPath::new(path).unwrap(), None, &CancelToken::new())
            .await
            .unwrap()
    }

    async fn youngest(&self) -> u64 {
        self.repo()
            .youngest_rev(&CancelToken::new())
            .await
            .unwrap()
            .0
    }

    async fn text_in_repo(&self, path: &str) -> String {
        let mut out = Vec::new();
        self.repo()
            .get_file(
                &RelPath::new(path).unwrap(),
                None,
                &mut out,
                &CancelToken::new(),
            )
            .await
            .unwrap();
        String::from_utf8(out).unwrap()
    }

    fn journals(&self) -> Vec<PathBuf> {
        JournalStore::new(self.f.engine.commit_journal_dir())
            .list()
            .unwrap_or_default()
    }

    async fn update(&self, wc: &WcRef) {
        self.f
            .engine
            .update(
                wc,
                &UpdateRequest {
                    wc_id: wc.id.clone(),
                    paths: None,
                    rev: None,
                    depth: None,
                },
                &ctx(),
            )
            .await
            .expect("update");
    }
}

fn request(wc: &WcRef, paths: &[&str], auto: bool) -> CommitRequest {
    CommitRequest {
        wc_id: wc.id.clone(),
        paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        message: "commit from the real engine".into(),
        keep_locks: false,
        auto_add_remove: auto,
    }
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn base_rev(root: &Path, rel: &str) -> Option<i64> {
    WcDb::open(root)
        .unwrap()
        .with_read(|c| {
            use rusqlite::OptionalExtension;
            c.query_row(
                "SELECT revision FROM nodes WHERE local_relpath = ?1 AND op_depth = 0",
                [rel],
                |r| r.get(0),
            )
            .optional()
            .map_err(svn_wc::db::sql_err)
        })
        .unwrap()
}

// ---- shared scenarios ----

/// The shared suite on the real engine; working copy files are written directly.
struct CommitHarness {
    env: Env,
}

impl Harness for CommitHarness {
    fn engine(&self) -> &dyn EngineFacade {
        &self.env.f.engine
    }
    fn repo_url(&self) -> String {
        URL.to_owned()
    }
    fn work_dir(&self) -> PathBuf {
        self.env.tmp.path().join("work")
    }
    fn write_file(&self, root: &Path, rel: &str, content: &str) -> Result<(), SvnError> {
        write(root, rel, content);
        Ok(())
    }
    fn remove_file(&self, root: &Path, rel: &str) -> Result<(), SvnError> {
        std::fs::remove_file(root.join(rel)).map_err(|e| SvnError::from_io("remove", &e))
    }
    fn scratch_dir(&self) -> PathBuf {
        self.env.tmp.path().to_path_buf()
    }
    fn plant_lock(&self, root: &Path, dir: &str, owner: PlantedOwner) -> Result<(), SvnError> {
        match owner {
            PlantedOwner::DeadProcess => plant_dead_app_lock(root, &self.env.f.store, dir),
            PlantedOwner::Foreign => plant_lock_row(root, dir),
        }
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_suite_commit_and_update_cycle() {
    conformance::commit_and_update_cycle(&CommitHarness { env: env() }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_suite_conflict_blocks_commit_until_resolved() {
    conformance::conflict_blocks_commit_until_resolved(&CommitHarness { env: env() }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_suite_schedule_and_revert() {
    conformance::schedule_and_revert(&CommitHarness { env: env() }).await;
}

// ---- auto add/remove ----

#[tokio::test(flavor = "multi_thread")]
async fn auto_add_remove_adds_unversioned_items_and_deletes_missing_ones() {
    let e = env();
    let wc = e.checkout("wc").await;
    write(&wc.root, "new.txt", "new\n");
    write(&wc.root, "newdir/inner.txt", "inner\n");
    // Selected file in a new folder: the folder is added and committed too.
    write(&wc.root, "src/deep/deeper.rs", "deeper\n");
    std::fs::remove_file(wc.root.join("README.md")).unwrap();

    let rev = e
        .commit(
            &wc,
            &["new.txt", "newdir", "README.md", "src/deep/deeper.rs"],
            true,
        )
        .await
        .expect("commit");
    assert_eq!(rev, Rev(2));
    assert_eq!(e.kind_in_repo("trunk/new.txt").await, NodeKind::File);
    assert_eq!(
        e.kind_in_repo("trunk/newdir/inner.txt").await,
        NodeKind::File
    );
    assert_eq!(e.kind_in_repo("trunk/src/deep").await, NodeKind::Dir);
    assert_eq!(
        e.kind_in_repo("trunk/src/deep/deeper.rs").await,
        NodeKind::File
    );
    assert_eq!(e.kind_in_repo("trunk/README.md").await, NodeKind::None);
    assert!(e.status(&wc).await.is_empty(), "{:?}", e.status(&wc).await);
    assert!(e.journals().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn without_auto_add_remove_an_unversioned_selection_is_refused_and_nothing_is_scheduled() {
    let e = env();
    let wc = e.checkout("wc").await;
    write(&wc.root, "new.txt", "new\n");
    e.commit(&wc, &["new.txt"], false)
        .await
        .expect_err("not versioned");
    assert_eq!(
        e.text_of(&wc, "new.txt").await,
        Some(NodeStatus::Unversioned)
    );
    assert_eq!(e.youngest().await, 1);

    let err = e
        .commit(&wc, &["README.md"], true)
        .await
        .expect_err("unchanged");
    assert_eq!(err.code, ErrorCode::Internal);
    assert_eq!(err.message, "nothing to commit");
    let err = e
        .commit(&wc, &["no-such-file"], true)
        .await
        .expect_err("missing");
    assert_eq!(err.code, ErrorCode::PathNotFound, "{err}");
}

/// Marks `rel` incomplete in BASE, as an interrupted checkout or update leaves it.
fn mark_incomplete(root: &Path, rel: &str) {
    let db = WcDb::open(root).unwrap();
    db.with_write(|tx| {
        tx.execute(
            "UPDATE nodes SET presence = 'incomplete' WHERE local_relpath = ?1 AND op_depth = 0",
            [rel],
        )
        .map(|_| ())
        .map_err(svn_wc::db::sql_err)
    })
    .unwrap();
}

/// Issue #99 item 2: content an interrupted checkout or update has not fetched yet looks missing
/// but is never scheduled for deletion, neither below an incomplete folder nor the incomplete
/// folder itself.
#[tokio::test(flavor = "multi_thread")]
async fn auto_add_remove_never_deletes_an_incomplete_folder_or_its_unfetched_content() {
    let e = env();
    let wc = e.checkout("wc").await;
    mark_incomplete(&wc.root, "src");
    std::fs::remove_file(wc.root.join("src/main.rs")).unwrap();
    write(&wc.root, "README.md", "changed\n");
    let rev = e.commit(&wc, &[""], true).await.expect("commit");
    assert_eq!(rev, Rev(2));
    assert_eq!(e.kind_in_repo("trunk/src/main.rs").await, NodeKind::File);
    assert_eq!(e.kind_in_repo("trunk/src").await, NodeKind::Dir);
    let st = e.status(&wc).await;
    assert!(
        st.iter()
            .all(|s| s.text != NodeStatus::Deleted && s.text != NodeStatus::Added),
        "{st:?}"
    );

    // The incomplete folder itself gone from disk: still never deleted.
    std::fs::remove_dir_all(wc.root.join("src")).unwrap();
    write(&wc.root, "README.md", "again\n");
    let rev = e.commit(&wc, &[""], true).await.expect("commit");
    assert_eq!(rev, Rev(3));
    assert_eq!(e.kind_in_repo("trunk/src").await, NodeKind::Dir);
    assert_eq!(e.kind_in_repo("trunk/src/main.rs").await, NodeKind::File);

    // An interrupted checkout marks the root: nothing below it is deleted either.
    let wc2 = e.checkout("wc2").await;
    mark_incomplete(&wc2.root, "");
    std::fs::remove_file(wc2.root.join("src/main.rs")).unwrap();
    write(&wc2.root, "new.txt", "new\n");
    let rev = e
        .commit(&wc2, &["", "new.txt"], true)
        .await
        .expect("commit");
    assert_eq!(rev, Rev(4));
    assert_eq!(e.kind_in_repo("trunk/src/main.rs").await, NodeKind::File);
    assert_eq!(e.kind_in_repo("trunk/new.txt").await, NodeKind::File);
}

/// Records `at` as a file external of `src` whose source is `trunk/README.md` (inside the
/// working copy's own subtree, the case where deleting it would hit a real repository path),
/// as svn 1.15 stores one: a BASE row with `file_external = 1` plus an `EXTERNALS` row.
fn plant_file_external(root: &Path, at: &str) {
    let db = WcDb::open(root).unwrap();
    db.with_write(|tx| {
        tx.execute(
            "INSERT INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, repos_id, \
             repos_path, revision, presence, kind, properties, checksum, changed_revision, \
             changed_date, changed_author, translated_size, last_mod_time, file_external) \
             SELECT wc_id, ?1, 0, 'src', repos_id, repos_path, revision, presence, kind, \
             properties, checksum, changed_revision, changed_date, changed_author, \
             translated_size, last_mod_time, 1 FROM nodes \
             WHERE local_relpath = 'README.md' AND op_depth = 0",
            [at],
        )
        .map_err(svn_wc::db::sql_err)?;
        tx.execute(
            "INSERT INTO externals (wc_id, local_relpath, parent_relpath, repos_id, presence, \
             kind, def_local_relpath, def_repos_relpath, def_operational_revision, \
             def_revision) SELECT wc_id, ?1, 'src', repos_id, 'normal', 'file', 'src', \
             repos_path, NULL, NULL FROM nodes \
             WHERE local_relpath = 'README.md' AND op_depth = 0",
            [at],
        )
        .map(|_| ())
        .map_err(svn_wc::db::sql_err)
    })
    .unwrap();
}

/// PR #105 review item 1: a file external whose file is gone from disk is reported missing, but
/// auto add/remove never schedules it for deletion: its BASE location is the external's source,
/// so the commit would delete that source from the repository.
#[tokio::test(flavor = "multi_thread")]
async fn auto_add_remove_never_deletes_the_source_of_a_missing_file_external() {
    let e = env();
    let wc = e.checkout("wc").await;
    plant_file_external(&wc.root, "src/shared.md");
    assert_eq!(
        e.text_of(&wc, "src/shared.md").await,
        Some(NodeStatus::Missing)
    );

    for paths in [&[""][..], &["src"], &["src/shared.md"]] {
        let err = e.commit(&wc, paths, true).await.expect_err("nothing");
        assert_eq!(err.message, "nothing to commit", "{paths:?}: {err}");
        assert_eq!(e.youngest().await, 1, "{paths:?}: the repository changed");
        assert_eq!(e.kind_in_repo("trunk/README.md").await, NodeKind::File);
    }
    // Nothing was scheduled: the external is still just missing, never deleted.
    assert_eq!(
        e.text_of(&wc, "src/shared.md").await,
        Some(NodeStatus::Missing)
    );

    // With another change in the selection the commit goes through without touching it.
    write(&wc.root, "src/main.rs", "changed\n");
    let rev = e.commit(&wc, &[""], true).await.expect("commit");
    assert_eq!(rev, Rev(2));
    assert_eq!(e.kind_in_repo("trunk/README.md").await, NodeKind::File);
    assert!(e.journals().is_empty());

    // Deleting it explicitly is refused as svn refuses it.
    let err =
        e.f.engine
            .delete_paths(&wc, &["src/shared.md".to_owned()], true, &ctx())
            .await
            .expect_err("external");
    assert_eq!(err.svn_code, Some(155_030), "{err}");
}

/// PR #105 re-review: an edited file external is committed only when it is named itself, as
/// svn 1.15 does (`svn ci src` and `svn ci .` send nothing); committing an enclosing folder,
/// even with auto add/remove, never writes the edit into the external's source.
#[tokio::test(flavor = "multi_thread")]
async fn commit_of_a_parent_never_sends_an_edited_file_external() {
    let e = env();
    let wc = e.checkout("wc").await;
    let source = e.text_in_repo("trunk/README.md").await;
    std::fs::copy(wc.root.join("README.md"), wc.root.join("src/shared.md")).unwrap();
    plant_file_external(&wc.root, "src/shared.md");
    write(&wc.root, "src/shared.md", "EDITED VIA EXTERNAL\n");
    assert_eq!(
        e.text_of(&wc, "src/shared.md").await,
        Some(NodeStatus::Modified)
    );

    for paths in [&["src"][..], &[""]] {
        let err = e.commit(&wc, paths, true).await.expect_err("nothing");
        assert_eq!(err.message, "nothing to commit", "{paths:?}: {err}");
        assert_eq!(e.youngest().await, 1, "{paths:?}: the repository changed");
        assert_eq!(e.text_in_repo("trunk/README.md").await, source);
    }

    // Alongside another change, only that change is committed.
    write(&wc.root, "src/main.rs", "changed\n");
    assert_eq!(e.commit(&wc, &[""], true).await.expect("commit"), Rev(2));
    assert_eq!(e.text_in_repo("trunk/src/main.rs").await, "changed\n");
    assert_eq!(e.text_in_repo("trunk/README.md").await, source);
    assert_eq!(
        e.text_of(&wc, "src/shared.md").await,
        Some(NodeStatus::Modified)
    );

    // Named explicitly, it is committed to its source, as `svn ci src/shared.md` does.
    assert_eq!(
        e.commit(&wc, &["src/shared.md"], true)
            .await
            .expect("commit"),
        Rev(3)
    );
    assert_eq!(
        e.text_in_repo("trunk/README.md").await,
        "EDITED VIA EXTERNAL\n"
    );
    assert!(e.journals().is_empty());
}

/// PR #105 review item 2: auto-add of an unversioned folder skips the working copies nested in
/// it at any depth (svn's W150002), which stay unversioned with all they contain, while their
/// siblings are added and committed.
#[tokio::test(flavor = "multi_thread")]
async fn auto_add_never_adds_a_nested_working_copy_at_any_depth() {
    let e = env();
    let wc = e.checkout("wc").await;
    for (name, at) in [("n1", "x/nested"), ("n2", "x/y/deeper")] {
        let other = e.checkout(name).await;
        std::fs::create_dir_all(wc.root.join(at).parent().unwrap()).unwrap();
        std::fs::rename(&other.root, wc.root.join(at)).unwrap();
        write(&wc.root, &format!("{at}/inner-local.txt"), "local\n");
    }
    write(&wc.root, "x/keep.txt", "keep\n");
    write(&wc.root, "x/y/also.txt", "also\n");

    let rev = e.commit(&wc, &["x"], true).await.expect("commit");
    assert_eq!(rev, Rev(2));
    assert_eq!(e.kind_in_repo("trunk/x/keep.txt").await, NodeKind::File);
    assert_eq!(e.kind_in_repo("trunk/x/y/also.txt").await, NodeKind::File);
    for gone in ["trunk/x/nested", "trunk/x/y/deeper"] {
        assert_eq!(e.kind_in_repo(gone).await, NodeKind::None, "{gone}");
    }
    assert_eq!(
        e.text_of(&wc, "x/nested").await,
        Some(NodeStatus::Unversioned)
    );
    assert_eq!(
        e.text_of(&wc, "x/y/deeper").await,
        Some(NodeStatus::Unversioned)
    );

    // A plain add behaves the same way.
    let other = e.checkout("n3").await;
    std::fs::create_dir_all(wc.root.join("z/q")).unwrap();
    std::fs::rename(&other.root, wc.root.join("z/q/nested")).unwrap();
    e.f.engine
        .add_paths(&wc, &["z".to_owned()], &ctx())
        .await
        .expect("add");
    assert_eq!(e.text_of(&wc, "z/q").await, Some(NodeStatus::Added));
    assert_eq!(
        e.text_of(&wc, "z/q/nested").await,
        Some(NodeStatus::Unversioned)
    );
    // And refuses the nested working copy as an explicit target.
    let err =
        e.f.engine
            .add_paths(&wc, &["z/q/nested".to_owned()], &ctx())
            .await
            .expect_err("nested");
    assert!(err.message.contains("is a nested working copy"), "{err}");
}

/// A conflicted `src/main.rs` with its marker files next to it.
async fn conflicted(e: &Env) -> WcRef {
    let wc = e.checkout("conflicted").await;
    e.repo()
        .seed(
            "alice",
            "theirs",
            &[put("trunk/src/main.rs", "fn main() { a(); }\n")],
        )
        .unwrap();
    write(&wc.root, "src/main.rs", "fn main() { b(); }\n");
    e.update(&wc).await;
    assert_eq!(
        e.text_of(&wc, "src/main.rs").await,
        Some(NodeStatus::Conflicted)
    );
    wc
}

fn markers(root: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(root.join("src"))
        .unwrap()
        .map(|d| d.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("main.rs."))
        .map(|n| format!("src/{n}"))
        .collect();
    found.sort();
    found
}

#[tokio::test(flavor = "multi_thread")]
async fn conflict_marker_files_are_never_added() {
    let e = env();
    let wc = conflicted(&e).await;
    let marker_files = markers(&wc.root);
    assert!(!marker_files.is_empty(), "svn wrote conflict markers");
    write(&wc.root, "new.txt", "new\n");

    // The conflict blocks the commit before anything is scheduled.
    let err = e
        .commit(&wc, &["", "new.txt"], true)
        .await
        .expect_err("conflicted");
    assert_eq!(err.code, ErrorCode::ConflictBlocksCommit, "{err}");
    assert_eq!(
        e.text_of(&wc, "new.txt").await,
        Some(NodeStatus::Unversioned)
    );
    for m in &marker_files {
        assert_eq!(
            e.text_of(&wc, m).await,
            Some(NodeStatus::Unversioned),
            "{m}"
        );
    }

    // A marker selected explicitly is skipped; the rest of the selection is committed.
    let mut selection = vec!["new.txt"];
    selection.extend(marker_files.iter().map(String::as_str));
    let rev = e.commit(&wc, &selection, true).await.expect("commit");
    assert_eq!(rev, Rev(3));
    assert_eq!(e.kind_in_repo("trunk/new.txt").await, NodeKind::File);
    for m in &marker_files {
        assert_eq!(
            e.text_of(&wc, m).await,
            Some(NodeStatus::Unversioned),
            "{m}"
        );
        assert_eq!(
            e.kind_in_repo(&format!("trunk/{m}")).await,
            NodeKind::None,
            "{m}"
        );
    }

    e.f.engine
        .resolve(
            &wc,
            &ResolveRequest {
                wc_id: wc.id.clone(),
                path: "src/main.rs".into(),
                choice: ResolveChoice::Working,
            },
            &ctx(),
        )
        .await
        .expect("resolve");
    let rev = e.commit(&wc, &[""], true).await.expect("commit resolved");
    assert_eq!(rev, Rev(4));
}

// ---- lost MERGE answer ----

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_merge_answer_of_an_applied_commit_is_settled_and_reported_as_committed() {
    let e = env();
    let wc = e.checkout("wc").await;
    write(&wc.root, "README.md", "changed\n");
    e.repo().fail_next_merge(FakeMergeFault::LostAfterMerge);
    let rev = e.commit(&wc, &["README.md"], true).await.expect("settled");
    assert_eq!(rev, Rev(2));
    assert_eq!(e.youngest().await, 2);
    assert!(e.journals().is_empty());
    assert_eq!(base_rev(&wc.root, "README.md"), Some(2));
    assert!(e.status(&wc).await.is_empty());
    assert!(
        e.connector.connects().contains(&false),
        "settled over an unattended session"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_merge_answer_of_a_commit_that_never_happened_can_be_repeated() {
    let e = env();
    let wc = e.checkout("wc").await;
    write(&wc.root, "README.md", "changed\n");
    e.repo().fail_next_merge(FakeMergeFault::LostBeforeMerge);
    let err = e
        .commit(&wc, &["README.md"], true)
        .await
        .expect_err("not applied");
    assert_eq!(err.code, ErrorCode::Network, "{err}");
    assert!(opensvn_ipc::AppError::from(&err).retryable);
    assert_eq!(e.youngest().await, 1);
    assert!(e.journals().is_empty());
    assert!(e.repo().open_txns().is_empty());
    assert_eq!(
        e.text_of(&wc, "README.md").await,
        Some(NodeStatus::Modified)
    );
    let rev = e.commit(&wc, &["README.md"], true).await.expect("again");
    assert_eq!(rev, Rev(2));
}

/// The commit is applied, its answer lost, and no unattended session is available to settle it:
/// the non-retryable error comes back, the journal (with the author) stays, the next commit is
/// refused, and opening the working copy later (through another spelling of its path) finalizes
/// it from the journal.
#[cfg(unix)]
async fn unsettled_commit_is_finalized_by_recovery(commit_via: &Path, open_via: &Path) {
    let e = env();
    let wc = e.checkout("wc").await;
    let real_root = std::fs::canonicalize(&wc.root).unwrap();
    let via = WcRef {
        id: wc.id.clone(),
        root: commit_via.to_path_buf(),
    };
    let _ = std::fs::remove_file(commit_via);
    let _ = std::fs::remove_file(open_via);
    link(&wc.root, commit_via);
    if open_via != commit_via {
        link(&wc.root, open_via);
    }

    write(&wc.root, "README.md", "changed\n");
    e.connector.offline_unattended.store(true, Ordering::SeqCst);
    e.repo().fail_next_merge(FakeMergeFault::LostAfterMerge);
    let err = e
        .commit(&via, &["README.md"], true)
        .await
        .expect_err("unknown");
    assert_eq!(err.code, ErrorCode::CommitOutcomeUnknown, "{err}");
    assert!(!opensvn_ipc::AppError::from(&err).retryable);
    assert_eq!(e.youngest().await, 2, "the server has the revision");
    let journals = e.journals();
    assert_eq!(journals.len(), 1);
    let data = JournalStore::read(&journals[0]).unwrap();
    assert!(data.outcome_unknown(), "{data:?}");
    assert_eq!(data.wcroot, real_root, "journals use the canonical root");
    assert_eq!(data.author.as_deref(), Some("tester"));

    write(&wc.root, "src/main.rs", "fn main() { next(); }\n");
    let refused = e
        .commit(&via, &["src/main.rs"], true)
        .await
        .expect_err("refused");
    assert_eq!(refused.code, ErrorCode::WcNeedsCleanup, "{refused}");
    assert_eq!(e.youngest().await, 2, "nothing committed twice");

    e.connector
        .offline_unattended
        .store(false, Ordering::SeqCst);
    let opened = e.f.engine.open_wc(open_via, &ctx()).await.expect("open");
    assert_eq!(opened.recovery.recovered, vec![String::new()], "{opened:?}");
    assert!(opened.recovery.needs_user_action.is_empty(), "{opened:?}");
    assert_eq!(
        Path::new(&opened.info.root_path),
        open_via,
        "the description keeps the caller's spelling"
    );
    assert!(e.journals().is_empty());
    assert_eq!(base_rev(&wc.root, "README.md"), Some(2));
    let rev = e
        .commit(&via, &["src/main.rs"], true)
        .await
        .expect("next commit");
    assert_eq!(rev, Rev(3));
}

#[cfg(unix)]
fn link(target: &Path, at: &Path) {
    if at != target {
        std::os::unix::fs::symlink(target, at).unwrap();
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn an_unsettled_commit_through_a_symlinked_root_is_finalized_by_recovery() {
    let links = tempfile::tempdir().unwrap();
    unsettled_commit_is_finalized_by_recovery(
        &links.path().join("commit-link"),
        &links.path().join("open-link"),
    )
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn an_unsettled_commit_is_finalized_when_opened_through_the_same_symlink() {
    let links = tempfile::tempdir().unwrap();
    let same = links.path().join("registered-link");
    unsettled_commit_is_finalized_by_recovery(&same, &same).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn copying_a_working_copy_to_a_url_is_refused_with_a_user_facing_error() {
    let e = env();
    let wc = e.checkout("wc").await;
    let err =
        e.f.engine
            .copy_location(
                &CopyLocationRequest {
                    from_url: format!("{URL}/trunk"),
                    from_rev: None,
                    to_url: format!("{URL}/branches/b"),
                    message: "branch".into(),
                    switch_wc_id: None,
                    from_wc: Some(wc.id.clone()),
                },
                Some(&wc),
                &ctx(),
            )
            .await
            .expect_err("unsupported");
    assert_eq!(err.code, ErrorCode::WcInvalidOperation, "{err}");
    assert!(err.message.contains("not supported"), "{err}");
}
