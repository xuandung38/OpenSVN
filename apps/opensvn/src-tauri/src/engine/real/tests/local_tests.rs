//! Local working copy operations of the real engine (WP5a): the local parts of the conformance
//! scenarios `schedule_and_revert`, `working_copy_metadata_operations` and
//! `conflict_blocks_commit_until_resolved`, run on working copies checked out with `svn-client`
//! from the in-memory repository (status read through `svn-client`, since the engine's status is
//! wired separately), plus path safety. `oracle` repeats the scenarios on working copies made and
//! checked by svn 1.15 when it is available.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use opensvn_ipc::{
    CleanupRequest, IgnoreAddRequest, IgnoreScope, ResolveChoice, ResolveRequest, RevertRequest,
    WcId,
};
use svn_client::StatusOptions;
use svn_ra_api::fake::FakeRaSession;
use svn_types::{
    CancelToken, Depth, ErrorCode, LockOwnershipStore, NodeStatus, PropStatus, RelPath, WcStatus,
};

use super::support::{
    Fixture, OLD_BOOT, StaticConnector, URL, UUID, ctx, fixture_on, plant_lock_row, put,
};
use crate::engine::facade::{EngineFacade, StatusQuery, WcRef};
use crate::engine::real::{ClientRecovery, read_local_ignores};

/// Repository with `README.md` and `src/main.rs` at its root (revision 1).
fn repo() -> FakeRaSession {
    let repo = FakeRaSession::new(URL, UUID).unwrap();
    repo.seed(
        "alice",
        "Initial import",
        &[
            put("README.md", "hello\n"),
            put("src/main.rs", "fn main() {}\n"),
        ],
    )
    .unwrap();
    repo
}

struct Env {
    f: Fixture,
    tmp: tempfile::TempDir,
}

fn env() -> Env {
    let repo = repo();
    let connector = Arc::new(StaticConnector(vec![repo.clone()]));
    Env {
        f: fixture_on(repo, connector, Arc::new(ClientRecovery)),
        tmp: tempfile::tempdir().unwrap(),
    }
}

impl Env {
    fn client(&self) -> svn_client::ClientCtx {
        self.f.engine.client_ctx(&ctx())
    }

    async fn checkout(&self, name: &str) -> WcRef {
        let path = self.tmp.path().join(name);
        svn_client::checkout(
            &self.client(),
            &self.f.repo,
            &path,
            None,
            Depth::Infinity,
            false,
            &CancelToken::new(),
        )
        .await
        .expect("checkout");
        WcRef {
            id: WcId(name.to_owned()),
            root: path.canonicalize().unwrap(),
        }
    }

    async fn update(&self, wc: &WcRef) {
        svn_client::update(
            &self.client(),
            &self.f.repo,
            &wc.root,
            None,
            Depth::Infinity,
            &CancelToken::new(),
        )
        .await
        .expect("update");
    }

    /// Status as the app would list it: unversioned items, svn's default ignores plus the
    /// working copy's local ignore list.
    async fn status(&self, wc: &WcRef) -> Vec<WcStatus> {
        let mut client = self.client();
        let mut globals: Vec<String> = svn_client::local::AddConfig::default().global_ignores;
        globals.extend(read_local_ignores(&wc.root).unwrap());
        client.config.global_ignores = Some(globals);
        svn_client::status(
            &client,
            &wc.root,
            &StatusOptions::default(),
            &CancelToken::new(),
        )
        .await
        .expect("status")
    }

    fn engine(&self) -> &dyn EngineFacade {
        &self.f.engine
    }
}

fn find<'a>(st: &'a [WcStatus], path: &str) -> Option<&'a WcStatus> {
    st.iter().find(|s| s.path.as_str() == path)
}

fn text_of(st: &[WcStatus], path: &str) -> Option<NodeStatus> {
    find(st, path).map(|s| s.text)
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn revert_req(wc: &WcRef, paths: &[&str], remove_unversioned: Option<bool>) -> RevertRequest {
    RevertRequest {
        wc_id: wc.id.clone(),
        paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        remove_unversioned,
    }
}

fn cleanup_req(wc: &WcRef, unversioned: bool, ignored: bool) -> CleanupRequest {
    CleanupRequest {
        wc_id: wc.id.clone(),
        remove_unversioned: Some(unversioned),
        remove_ignored: Some(ignored),
        vacuum_pristines: Some(false),
    }
}

fn paths(p: &[&str]) -> Vec<String> {
    p.iter().map(|s| (*s).to_owned()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn schedule_and_revert() {
    let e = env();
    let wc = e.checkout("schedule").await;
    let eng = e.engine();
    write(&wc.root, "new.txt", "new\n");
    assert_eq!(
        text_of(&e.status(&wc).await, "new.txt"),
        Some(NodeStatus::Unversioned)
    );
    eng.add_paths(&wc, &paths(&["new.txt"]), &ctx())
        .await
        .expect("add");
    assert_eq!(
        text_of(&e.status(&wc).await, "new.txt"),
        Some(NodeStatus::Added)
    );
    eng.revert(&wc, &revert_req(&wc, &["new.txt"], None), &ctx())
        .await
        .expect("revert add");
    assert_eq!(
        text_of(&e.status(&wc).await, "new.txt"),
        Some(NodeStatus::Unversioned)
    );
    assert!(wc.root.join("new.txt").exists(), "revert keeps added files");

    eng.delete_paths(&wc, &paths(&["README.md"]), false, &ctx())
        .await
        .expect("delete");
    assert_eq!(
        text_of(&e.status(&wc).await, "README.md"),
        Some(NodeStatus::Deleted)
    );
    assert!(!wc.root.join("README.md").exists());
    eng.revert(&wc, &revert_req(&wc, &["README.md"], None), &ctx())
        .await
        .expect("revert delete");
    assert!(find(&e.status(&wc).await, "README.md").is_none());
    assert_eq!(
        std::fs::read_to_string(wc.root.join("README.md")).unwrap(),
        "hello\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn add_schedules_parents_and_contents() {
    let e = env();
    let wc = e.checkout("add").await;
    write(&wc.root, "docs/guide/intro.md", "intro\n");
    write(&wc.root, "docs/guide/debug.o", "obj\n");
    e.engine()
        .add_paths(&wc, &paths(&["docs/guide"]), &ctx())
        .await
        .expect("add with parents");
    let st = e.status(&wc).await;
    assert_eq!(text_of(&st, "docs"), Some(NodeStatus::Added));
    assert_eq!(text_of(&st, "docs/guide"), Some(NodeStatus::Added));
    assert_eq!(text_of(&st, "docs/guide/intro.md"), Some(NodeStatus::Added));
    assert_eq!(
        text_of(&st, "docs/guide/debug.o"),
        None,
        "globally ignored files are not added"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_refuses_local_changes_and_can_keep_files() {
    let e = env();
    let wc = e.checkout("delete").await;
    write(&wc.root, "README.md", "changed\n");
    let err = e
        .engine()
        .delete_paths(&wc, &paths(&["README.md"]), false, &ctx())
        .await
        .expect_err("modified files are not deleted without force");
    assert_eq!(err.code, ErrorCode::WcInvalidOperation);
    assert_eq!(
        std::fs::read_to_string(wc.root.join("README.md")).unwrap(),
        "changed\n"
    );

    e.engine()
        .delete_paths(&wc, &paths(&["src/main.rs"]), true, &ctx())
        .await
        .expect("delete keep local");
    assert_eq!(
        text_of(&e.status(&wc).await, "src/main.rs"),
        Some(NodeStatus::Deleted)
    );
    assert!(
        wc.root.join("src/main.rs").exists(),
        "keep_local leaves the file"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn revert_can_remove_unversioned_items_but_keeps_reverted_additions() {
    let e = env();
    let wc = e.checkout("revert-clean").await;
    write(&wc.root, "src/added.rs", "added\n");
    e.engine()
        .add_paths(&wc, &paths(&["src/added.rs"]), &ctx())
        .await
        .unwrap();
    write(&wc.root, "src/main.rs", "changed\n");
    write(&wc.root, "src/scratch.txt", "scratch\n");
    write(&wc.root, "src/tmp/deep.txt", "deep\n");
    write(&wc.root, "src/debug.o", "ignored\n");
    write(&wc.root, "outside-target.txt", "keep\n");

    e.engine()
        .revert(&wc, &revert_req(&wc, &["src"], Some(true)), &ctx())
        .await
        .expect("revert");
    assert_eq!(
        std::fs::read_to_string(wc.root.join("src/main.rs")).unwrap(),
        "fn main() {}\n"
    );
    assert!(!wc.root.join("src/scratch.txt").exists());
    assert!(!wc.root.join("src/tmp").exists());
    assert!(wc.root.join("src/debug.o").exists(), "ignored items stay");
    assert!(
        wc.root.join("src/added.rs").exists(),
        "reverted additions stay"
    );
    assert!(
        wc.root.join("outside-target.txt").exists(),
        "only below the targets"
    );
    let st = e.status(&wc).await;
    assert_eq!(text_of(&st, "src/added.rs"), Some(NodeStatus::Unversioned));
}

/// Working copy whose root defines `ext` (and `deep/x/ext`) with `svn:externals`, both present
/// as separate working copies with a local edit, plus a separate checkout `nested`, an
/// unversioned directory hiding another checkout (`junk/inner`), and plain unversioned and
/// ignored items.
async fn wc_with_externals(e: &Env, name: &str) -> WcRef {
    let wc = e.checkout(name).await;
    e.engine()
        .props_set(
            &wc,
            "",
            "svn:externals",
            Some("^/src ext\n-r1 ^/src deep/x/ext\n"),
            &ctx(),
        )
        .await
        .expect("propset");
    for sub in ["ext", "deep/x/ext", "nested", "junk/inner"] {
        e.checkout(&format!("{name}/{sub}")).await;
        write(&wc.root, &format!("{sub}/README.md"), "local edit\n");
    }
    write(&wc.root, "junk/scratch.txt", "x\n");
    write(&wc.root, "scratch.txt", "x\n");
    write(&wc.root, "debug.o", "x\n");
    wc
}

/// The externals and the nested checkouts are where they were, local edits included.
fn assert_working_copies_kept(wc: &WcRef) {
    for sub in ["ext", "deep/x/ext", "nested", "junk/inner"] {
        assert_eq!(
            std::fs::read_to_string(wc.root.join(sub).join("README.md")).unwrap(),
            "local edit\n",
            "{sub} kept with its local edit"
        );
        assert!(
            wc.root.join(sub).join(".svn/wc.db").exists(),
            "{sub} still a working copy"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn status_reports_externals_and_nested_checkouts() {
    let e = env();
    let wc = wc_with_externals(&e, "ext-status").await;
    let st = e.status(&wc).await;
    assert_eq!(text_of(&st, "ext"), Some(NodeStatus::External));
    assert_eq!(text_of(&st, "deep"), Some(NodeStatus::External));
    assert_eq!(
        text_of(&st, "nested"),
        Some(NodeStatus::Unversioned),
        "as svn shows it"
    );
    assert_eq!(text_of(&st, "junk"), Some(NodeStatus::Unversioned));
    assert!(
        find(&st, "ext/README.md").is_none(),
        "externals are not descended into"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cleanup_removing_unversioned_and_ignored_keeps_externals_and_nested_checkouts() {
    let e = env();
    let wc = wc_with_externals(&e, "ext-cleanup").await;
    e.engine()
        .cleanup(&wc, &cleanup_req(&wc, true, true), &ctx())
        .await
        .expect("cleanup");
    assert_working_copies_kept(&wc);
    assert!(!wc.root.join("scratch.txt").exists());
    assert!(!wc.root.join("debug.o").exists());
    assert!(
        !wc.root.join("junk/scratch.txt").exists(),
        "the rest of a directory hiding a checkout goes"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn revert_removing_unversioned_keeps_externals_and_nested_checkouts() {
    let e = env();
    let wc = wc_with_externals(&e, "ext-revert").await;
    e.engine()
        .revert(&wc, &revert_req(&wc, &[""], Some(true)), &ctx())
        .await
        .expect("revert root");
    // Named directly as targets they are refused, and kept.
    for target in ["ext", "deep/x/ext", "nested", "junk/inner"] {
        let err = e
            .engine()
            .revert(&wc, &revert_req(&wc, &[target], Some(true)), &ctx())
            .await
            .expect_err(target);
        assert_eq!(err.code, ErrorCode::WcInvalidOperation, "{target}");
        assert!(
            err.message.contains("open that working copy"),
            "{target}: {}",
            err.message
        );
    }
    // A directory leading to an external is this working copy's; it is kept as it is.
    e.engine()
        .revert(&wc, &revert_req(&wc, &["deep"], Some(true)), &ctx())
        .await
        .expect("revert the directory leading to an external");
    assert_working_copies_kept(&wc);
    assert!(!wc.root.join("scratch.txt").exists());
    assert!(wc.root.join("debug.o").exists(), "ignored items stay");
}

/// Revert of `rel` with `remove_unversioned` fails with the "other working copy" error and
/// leaves `kept` (relative to the working copy root) on disk with its content.
async fn assert_revert_refused(e: &Env, wc: &WcRef, rel: &str, kept: &str) {
    let before = std::fs::read(wc.root.join(kept)).unwrap();
    let err = e
        .engine()
        .revert(wc, &revert_req(wc, &[rel], Some(true)), &ctx())
        .await
        .expect_err(rel);
    assert_eq!(err.code, ErrorCode::WcInvalidOperation, "{rel}");
    assert!(
        err.message.contains("open that working copy"),
        "{rel}: {}",
        err.message
    );
    assert_eq!(
        std::fs::read(wc.root.join(kept)).unwrap(),
        before,
        "{kept} kept after reverting {rel}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reverting_a_path_inside_an_external_or_nested_checkout_is_refused() {
    let e = env();
    let wc = wc_with_externals(&e, "inside-other-wc").await;
    write(&wc.root, "ext/src/main.rs", "edited in the external\n");
    assert_revert_refused(&e, &wc, "ext/README.md", "ext/README.md").await;
    assert_revert_refused(&e, &wc, "ext/src", "ext/src/main.rs").await;
    assert_revert_refused(&e, &wc, "deep/x/ext/README.md", "deep/x/ext/README.md").await;
    assert_revert_refused(&e, &wc, "nested/README.md", "nested/README.md").await;
    assert_revert_refused(&e, &wc, "junk/inner/src", "junk/inner/src/main.rs").await;
    assert_revert_refused(&e, &wc, "junk/inner/README.md", "junk/inner/README.md").await;
    // Without `remove_unversioned` too: nothing is skipped silently.
    let err = e
        .engine()
        .revert(&wc, &revert_req(&wc, &["nested/README.md"], None), &ctx())
        .await
        .expect_err("plain revert inside a nested checkout");
    assert!(err.message.contains("'nested'"), "{}", err.message);
    assert_working_copies_kept(&wc);
    // The rest of an unversioned directory is still this working copy's.
    e.engine()
        .revert(
            &wc,
            &revert_req(&wc, &["junk/scratch.txt"], Some(true)),
            &ctx(),
        )
        .await
        .expect("revert an unversioned file beside a nested checkout");
    assert!(!wc.root.join("junk/scratch.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn other_operations_refuse_paths_inside_an_external_or_nested_checkout() {
    let e = env();
    let wc = wc_with_externals(&e, "ops-other-wc").await;
    let eng = e.engine();
    write(&wc.root, "ext/new.txt", "x\n");
    let err = eng
        .add_paths(&wc, &paths(&["ext/new.txt"]), &ctx())
        .await
        .expect_err("add inside an external");
    assert_eq!(err.code, ErrorCode::WcInvalidOperation);
    let err = eng
        .delete_paths(&wc, &paths(&["nested/README.md"]), false, &ctx())
        .await
        .expect_err("delete inside a nested checkout");
    assert_eq!(err.code, ErrorCode::WcInvalidOperation);
    let err = eng
        .move_path(&wc, "README.md", "ext/moved.md", &ctx())
        .await
        .expect_err("move into an external");
    assert_eq!(err.code, ErrorCode::WcInvalidOperation);
    assert!(wc.root.join("README.md").exists());
    assert_working_copies_kept(&wc);
}

/// A directory defined by `svn:externals` but not fetched (no administrative directory) is
/// protected by the definition alone: status shows it as an external, cleanup and revert keep
/// it and a path inside it is refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_defined_but_unfetched_external_directory_is_kept() {
    let e = env();
    let wc = e.checkout("unfetched-ext").await;
    e.engine()
        .props_set(
            &wc,
            "src",
            "svn:externals",
            Some("^/lib vendor/lib\n"),
            &ctx(),
        )
        .await
        .expect("propset");
    write(&wc.root, "src/vendor/lib/user.txt", "precious\n");
    write(&wc.root, "scratch.txt", "x\n");
    assert!(!wc.root.join("src/vendor/lib/.svn").exists());
    assert_eq!(
        text_of(&e.status(&wc).await, "src/vendor"),
        Some(NodeStatus::External)
    );
    assert_revert_refused(
        &e,
        &wc,
        "src/vendor/lib/user.txt",
        "src/vendor/lib/user.txt",
    )
    .await;
    assert_revert_refused(&e, &wc, "src/vendor/lib", "src/vendor/lib/user.txt").await;
    // Reverting `src` itself would also revert its (local) definition, so only the unversioned
    // directory leading to the external is named.
    e.engine()
        .revert(&wc, &revert_req(&wc, &["src/vendor"], Some(true)), &ctx())
        .await
        .expect("revert the directory leading to the external");
    e.engine()
        .cleanup(&wc, &cleanup_req(&wc, true, true), &ctx())
        .await
        .expect("cleanup");
    assert_eq!(
        std::fs::read_to_string(wc.root.join("src/vendor/lib/user.txt")).unwrap(),
        "precious\n"
    );
    assert!(!wc.root.join("scratch.txt").exists(), "cleanup did run");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn removing_unversioned_items_continues_past_failures_and_reports_them() {
    let e = env();
    let wc = e.checkout("remove-failures").await;
    write(&wc.root, "a.txt", "x\n");
    write(&wc.root, "z.txt", "x\n");
    write(&wc.root, "real/inside.txt", "x\n");
    std::os::unix::fs::symlink(wc.root.join("real"), wc.root.join("link")).unwrap();
    let db = crate::engine::real::wc_read::open_db(&wc.root).unwrap();
    let rels: Vec<RelPath> = ["a.txt", "link/inside.txt", "z.txt"]
        .iter()
        .map(|p| RelPath::new(p).unwrap())
        .collect();
    let err = crate::engine::real::local_util::remove_items(&db, &CancelToken::new(), &rels)
        .expect_err("the path through a symlink is refused");
    assert!(err.message.contains("1 of 3"), "{}", err.message);
    assert!(err.message.contains("link/inside.txt"), "{}", err.message);
    assert!(!wc.root.join("a.txt").exists());
    assert!(
        !wc.root.join("z.txt").exists(),
        "items after the failure are removed"
    );
    assert!(wc.root.join("real/inside.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn working_copy_metadata_operations() {
    let e = env();
    let wc = e.checkout("metadata").await;
    let eng = e.engine();

    eng.props_set(&wc, "README.md", "svn:needs-lock", Some("*"), &ctx())
        .await
        .expect("props_set");
    let st = e.status(&wc).await;
    let readme = find(&st, "README.md").expect("property change is listed");
    assert!(readme.needs_lock);
    assert_eq!(readme.props, PropStatus::Modified);
    eng.props_set(&wc, "README.md", "svn:needs-lock", None, &ctx())
        .await
        .expect("props delete");
    assert!(find(&e.status(&wc).await, "README.md").is_none());

    eng.changelist_set(&wc, &paths(&["README.md"]), Some("docs"), &ctx())
        .await
        .expect("changelist");
    let db = svn_wc::WcDb::open(&wc.root).unwrap();
    let readme = RelPath::new("README.md").unwrap();
    assert_eq!(db.changelist(&readme).unwrap().as_deref(), Some("docs"));
    eng.changelist_set(&wc, &paths(&["README.md"]), None, &ctx())
        .await
        .expect("clear changelist");
    assert_eq!(db.changelist(&readme).unwrap(), None);
    let err = eng
        .changelist_set(&wc, &paths(&["README.md"]), Some("  "), &ctx())
        .await
        .expect_err("blank changelist name");
    assert_eq!(err.code, ErrorCode::WcInvalidOperation);

    write(&wc.root, "build.log", "x\n");
    eng.ignore_add(
        &wc,
        &IgnoreAddRequest {
            wc_id: wc.id.clone(),
            pattern: "*.log".into(),
            dir: String::new(),
            scope: IgnoreScope::Local,
        },
        &ctx(),
    )
    .await
    .expect("ignore_add local");
    let st = e.status(&wc).await;
    assert!(
        find(&st, "build.log").is_none(),
        "ignored files are not listed"
    );
    let listed = eng
        .status(
            &wc,
            StatusQuery {
                include_ignored: false,
                include_unversioned: true,
            },
            &ctx(),
        )
        .await
        .expect("engine status");
    assert!(
        !listed.iter().any(|f| f.path == "build.log"),
        "the engine's status applies the local ignore list: {listed:?}"
    );
    assert!(
        find(&st, "").is_none(),
        "a local ignore changes nothing versioned"
    );

    write(&wc.root, "src/out.tmp", "x\n");
    eng.ignore_add(
        &wc,
        &IgnoreAddRequest {
            wc_id: wc.id.clone(),
            pattern: "out.tmp".into(),
            dir: "src".into(),
            scope: IgnoreScope::SvnIgnore,
        },
        &ctx(),
    )
    .await
    .expect("ignore_add svn:ignore");
    let st = e.status(&wc).await;
    assert!(find(&st, "src/out.tmp").is_none());
    assert_eq!(find(&st, "src").unwrap().props, PropStatus::Modified);
    let err = eng
        .ignore_add(
            &wc,
            &IgnoreAddRequest {
                wc_id: wc.id.clone(),
                pattern: "a/b".into(),
                dir: String::new(),
                scope: IgnoreScope::SvnIgnore,
            },
            &ctx(),
        )
        .await
        .expect_err("patterns are names");
    assert_eq!(err.code, ErrorCode::WcInvalidOperation);

    eng.move_path(&wc, "README.md", "GUIDE.md", &ctx())
        .await
        .expect("move");
    let st = e.status(&wc).await;
    assert_eq!(text_of(&st, "GUIDE.md"), Some(NodeStatus::Added));
    assert_eq!(text_of(&st, "README.md"), Some(NodeStatus::Deleted));
    assert_eq!(
        find(&st, "GUIDE.md")
            .unwrap()
            .moved_from
            .as_ref()
            .map(RelPath::as_str),
        Some("README.md")
    );

    let err = eng
        .mark_move(&wc, "README.md", "GUIDE.md", &ctx())
        .await
        .expect_err("not supported yet");
    assert_eq!(err.code, ErrorCode::WcInvalidOperation);
    assert!(err.message.contains("not supported yet"), "{}", err.message);

    write(&wc.root, "scratch.txt", "x\n");
    eng.cleanup(&wc, &cleanup_req(&wc, true, true), &ctx())
        .await
        .expect("cleanup");
    assert!(!wc.root.join("scratch.txt").exists());
    assert!(
        !wc.root.join("build.log").exists(),
        "locally ignored is removed too"
    );
    assert!(!wc.root.join("src/out.tmp").exists());
    assert!(wc.root.join("GUIDE.md").exists(), "versioned content stays");

    eng.upgrade_format(&wc, 31, &ctx())
        .await
        .expect("upgrade to the current format is a no-op");
    let err = eng
        .upgrade_format(&wc, 32, &ctx())
        .await
        .expect_err("format change not supported yet");
    assert_eq!(err.code, ErrorCode::WcFormatUnsupported);
}

#[tokio::test(flavor = "multi_thread")]
async fn cleanup_keeps_ignored_items_unless_asked() {
    let e = env();
    let wc = e.checkout("cleanup").await;
    write(&wc.root, "scratch.txt", "x\n");
    write(&wc.root, "debug.o", "x\n");
    e.engine()
        .cleanup(&wc, &cleanup_req(&wc, false, false), &ctx())
        .await
        .expect("plain cleanup");
    assert!(wc.root.join("scratch.txt").exists());
    e.engine()
        .cleanup(&wc, &cleanup_req(&wc, true, false), &ctx())
        .await
        .expect("cleanup unversioned");
    assert!(!wc.root.join("scratch.txt").exists());
    assert!(wc.root.join("debug.o").exists());
    e.engine()
        .cleanup(&wc, &cleanup_req(&wc, false, true), &ctx())
        .await
        .expect("cleanup ignored");
    assert!(!wc.root.join("debug.o").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn cleanup_reports_a_lock_that_needs_the_user_as_busy() {
    let e = env();
    let wc = e.checkout("busy").await;
    // A dead app process is on record for `src` but no marker proves the row is its own.
    plant_lock_row(&wc.root, "src");
    e.f.store
        .record(&wc.root, &RelPath::new("src").unwrap(), 4242, OLD_BOOT)
        .unwrap();
    write(&wc.root, "scratch.txt", "x\n");
    let err = e
        .engine()
        .cleanup(&wc, &cleanup_req(&wc, true, true), &ctx())
        .await
        .expect_err("locked");
    assert_eq!(err.code, ErrorCode::WcBusy);
    assert!(wc.root.join("scratch.txt").exists(), "nothing removed");
}

async fn conflicted_pair(e: &Env) -> WcRef {
    let b = e.checkout("conflict-b").await;
    e.f.repo
        .seed(
            "alice",
            "from a",
            &[put("src/main.rs", "fn main() { a(); }\n")],
        )
        .unwrap();
    write(&b.root, "src/main.rs", "fn main() { b(); }\n");
    e.update(&b).await;
    assert_eq!(
        text_of(&e.status(&b).await, "src/main.rs"),
        Some(NodeStatus::Conflicted)
    );
    b
}

fn resolve_req(wc: &WcRef, choice: ResolveChoice) -> ResolveRequest {
    ResolveRequest {
        wc_id: wc.id.clone(),
        path: "src/main.rs".into(),
        choice,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn conflict_resolved_with_theirs_leaves_nothing_to_commit() {
    let e = env();
    let b = conflicted_pair(&e).await;
    e.engine()
        .resolve(&b, &resolve_req(&b, ResolveChoice::Theirs), &ctx())
        .await
        .expect("resolve");
    let st = e.status(&b).await;
    assert!(find(&st, "src/main.rs").is_none(), "{st:?}");
    assert!(
        st.iter().all(|s| s.text != NodeStatus::Unversioned),
        "conflict markers are removed: {st:?}"
    );
    assert_eq!(
        std::fs::read_to_string(b.root.join("src/main.rs")).unwrap(),
        "fn main() { a(); }\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn conflict_choices_map_to_svn_accept_values() {
    for (choice, expected) in [
        (ResolveChoice::Mine, Some("fn main() { b(); }\n")),
        (ResolveChoice::Base, Some("fn main() {}\n")),
        (ResolveChoice::Working, None),
    ] {
        let e = env();
        let b = conflicted_pair(&e).await;
        let before = std::fs::read_to_string(b.root.join("src/main.rs")).unwrap();
        e.engine()
            .resolve(&b, &resolve_req(&b, choice), &ctx())
            .await
            .expect("resolve");
        let after = std::fs::read_to_string(b.root.join("src/main.rs")).unwrap();
        assert_eq!(after, expected.map_or(before, str::to_owned), "{choice:?}");
        let st = e.status(&b).await;
        assert_ne!(
            text_of(&st, "src/main.rs"),
            Some(NodeStatus::Conflicted),
            "{choice:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn paths_outside_the_working_copy_are_refused() {
    let e = env();
    let wc = e.checkout("safety").await;
    let outside = e.tmp.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("victim.txt"), "keep\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, wc.root.join("link")).unwrap();

    let mut bad = vec![
        "../outside/victim.txt".to_owned(),
        outside.join("victim.txt").to_string_lossy().into_owned(),
        "src/../../outside/victim.txt".to_owned(),
        ".svn/wc.db".to_owned(),
        "src//main.rs".to_owned(),
    ];
    if cfg!(unix) {
        bad.push("link/victim.txt".to_owned());
    }
    let eng = e.engine();
    for p in &bad {
        let one = vec![p.clone()];
        let errs = [
            eng.add_paths(&wc, &one, &ctx()).await.err(),
            eng.delete_paths(&wc, &one, false, &ctx()).await.err(),
            eng.revert(&wc, &revert_req(&wc, &[p], Some(true)), &ctx())
                .await
                .err(),
            eng.changelist_set(&wc, &one, Some("x"), &ctx()).await.err(),
            eng.props_set(&wc, p, "p", Some("v"), &ctx()).await.err(),
            eng.move_path(&wc, "README.md", p, &ctx()).await.err(),
            eng.move_path(&wc, p, "moved.txt", &ctx()).await.err(),
            eng.resolve(
                &wc,
                &ResolveRequest {
                    wc_id: wc.id.clone(),
                    path: p.clone(),
                    choice: ResolveChoice::Working,
                },
                &ctx(),
            )
            .await
            .err(),
        ];
        for (i, err) in errs.into_iter().enumerate() {
            let err = err.unwrap_or_else(|| panic!("{p:?} accepted by call {i}"));
            assert_eq!(err.code, ErrorCode::UnsafePath, "{p:?} call {i}: {err}");
        }
    }
    let ignore_dir = eng
        .ignore_add(
            &wc,
            &IgnoreAddRequest {
                wc_id: wc.id.clone(),
                pattern: "*.x".into(),
                dir: "../outside".into(),
                scope: IgnoreScope::SvnIgnore,
            },
            &ctx(),
        )
        .await
        .expect_err("dir outside");
    assert_eq!(ignore_dir.code, ErrorCode::UnsafePath);

    assert_eq!(
        std::fs::read_to_string(outside.join("victim.txt")).unwrap(),
        "keep\n"
    );
    assert!(wc.root.join("README.md").exists());
    let st = e.status(&wc).await;
    assert!(
        st.iter()
            .all(|s| matches!(s.text, NodeStatus::Unversioned | NodeStatus::Normal)),
        "nothing scheduled: {st:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_directory_inside_a_working_copy_is_not_a_root() {
    let e = env();
    let wc = e.checkout("nested").await;
    let inner = WcRef {
        id: wc.id.clone(),
        root: wc.root.join("src"),
    };
    let err = e
        .engine()
        .add_paths(&inner, &paths(&["x"]), &ctx())
        .await
        .expect_err("not a root");
    assert_eq!(err.code, ErrorCode::NotAWorkingCopy);
}

#[tokio::test(flavor = "multi_thread")]
async fn configured_global_ignores_and_auto_props_apply_to_add() {
    let e = env();
    let wc = e.checkout("config").await;
    let cfg = e.tmp.path().join("svn-config");
    std::fs::create_dir(&cfg).unwrap();
    std::fs::write(
        cfg.join("config"),
        "[miscellany]\nglobal-ignores = *.bak\nenable-auto-props = yes\n[auto-props]\n*.sh = svn:eol-style=LF\n",
    )
    .unwrap();
    e.f.engine.set_subversion_config_dir(Some(cfg));
    write(&wc.root, "tools/run.sh", "echo\n");
    write(&wc.root, "tools/old.bak", "x\n");
    write(&wc.root, "tools/debug.o", "x\n");
    e.engine()
        .add_paths(&wc, &paths(&["tools"]), &ctx())
        .await
        .expect("add");
    let db = svn_wc::WcDb::open(&wc.root).unwrap();
    let sh = RelPath::new("tools/run.sh").unwrap();
    assert_eq!(
        svn_client::local::propget(&db, &sh, "svn:eol-style")
            .unwrap()
            .as_deref(),
        Some(&b"LF"[..])
    );
    assert!(
        db.read_node(&RelPath::new("tools/old.bak").unwrap())
            .unwrap()
            .is_none()
    );
    assert!(
        db.read_node(&RelPath::new("tools/debug.o").unwrap())
            .unwrap()
            .is_some(),
        "the configured list replaces svn's default list"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_calls_change_nothing() {
    let e = env();
    let wc = e.checkout("cancel").await;
    write(&wc.root, "new.txt", "x\n");
    let c = ctx();
    c.cancel.cancel();
    let err = e
        .engine()
        .add_paths(&wc, &paths(&["new.txt"]), &c)
        .await
        .expect_err("cancelled");
    assert_eq!(err.code, ErrorCode::Cancelled);
    assert_eq!(
        text_of(&e.status(&wc).await, "new.txt"),
        Some(NodeStatus::Unversioned)
    );
}

/// The same scenarios on working copies created and inspected by svn 1.15.
mod oracle {
    use std::process::Command;

    use opensvn_svn_oracle::{MinVersion, Oracle};

    use super::*;

    struct Repo {
        oracle: Oracle,
        _dir: tempfile::TempDir,
        base: PathBuf,
        url: String,
    }

    fn run(mut cmd: Command, cwd: &Path) -> String {
        cmd.current_dir(cwd);
        let out = cmd.output().expect("spawn");
        assert!(
            out.status.success(),
            "{cmd:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    impl Repo {
        fn new() -> Option<Self> {
            let oracle = Oracle::resolve(MinVersion::V1_15).expect("oracle")?;
            let dir = tempfile::tempdir().unwrap();
            let base = dir.path().canonicalize().unwrap();
            let repo = base.join("repo");
            let mut c = oracle.svnadmin();
            c.arg("create").arg(&repo);
            run(c, &base);
            let url = Oracle::file_url(&repo);
            let seed = base.join("seed");
            write(&seed, "trunk/README.md", "hello\n");
            write(&seed, "trunk/src/main.rs", "fn main() {}\n");
            let r = Self {
                oracle,
                _dir: dir,
                base,
                url,
            };
            r.svn(&r.base, &["import", "-q", "-m", "init", "seed", &r.url]);
            Some(r)
        }

        fn svn(&self, cwd: &Path, args: &[&str]) -> String {
            let mut c = self.oracle.svn();
            c.args(args);
            run(c, cwd)
        }

        fn checkout(&self, name: &str) -> WcRef {
            let url = format!("{}/trunk", self.url);
            self.svn(&self.base, &["checkout", "-q", &url, name]);
            WcRef {
                id: WcId(name.to_owned()),
                root: self.base.join(name),
            }
        }

        /// `svn status` lines, sorted (`--no-ignore` off; svn's default ignores).
        fn status(&self, wc: &WcRef) -> Vec<String> {
            let mut lines: Vec<String> = self
                .svn(&wc.root, &["status"])
                .lines()
                .map(str::to_owned)
                .collect();
            lines.sort();
            lines
        }
    }

    fn engine() -> Fixture {
        let repo = super::repo();
        let connector = Arc::new(StaticConnector(vec![repo.clone()]));
        fixture_on(repo, connector, Arc::new(ClientRecovery))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn schedule_and_revert_matches_svn() {
        let Some(r) = Repo::new() else { return };
        let f = engine();
        let eng: &dyn EngineFacade = &f.engine;
        let wc = r.checkout("schedule");
        write(&wc.root, "new.txt", "new\n");
        write(&wc.root, "lib/inner.rs", "x\n");
        eng.add_paths(&wc, &paths(&["new.txt", "lib/inner.rs"]), &ctx())
            .await
            .expect("add");
        assert_eq!(
            r.status(&wc),
            vec!["A       lib", "A       lib/inner.rs", "A       new.txt"]
        );
        eng.delete_paths(&wc, &paths(&["README.md"]), false, &ctx())
            .await
            .expect("delete");
        eng.move_path(&wc, "src/main.rs", "src/app.rs", &ctx())
            .await
            .expect("move");
        assert_eq!(
            r.status(&wc),
            vec![
                "A       lib",
                "A       lib/inner.rs",
                "A       new.txt",
                "A  +    src/app.rs",
                "        > moved from src/main.rs",
                "D       README.md",
                "D       src/main.rs",
                "        > moved to src/app.rs",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
            .tap_sort()
        );
        eng.revert(&wc, &revert_req(&wc, &[""], None), &ctx())
            .await
            .expect("revert all");
        assert_eq!(
            r.status(&wc),
            vec!["?       lib", "?       new.txt", "?       src/app.rs"]
        );
        assert_eq!(
            std::fs::read_to_string(wc.root.join("README.md")).unwrap(),
            "hello\n"
        );
        r.svn(&wc.root, &["cleanup"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn metadata_operations_match_svn() {
        let Some(r) = Repo::new() else { return };
        let f = engine();
        let eng: &dyn EngineFacade = &f.engine;
        let wc = r.checkout("metadata");
        eng.props_set(&wc, "README.md", "svn:needs-lock", Some("*"), &ctx())
            .await
            .expect("props_set");
        assert_eq!(r.status(&wc), vec![" M      README.md"]);
        assert_eq!(
            r.svn(
                &wc.root,
                &["propget", "--no-newline", "svn:needs-lock", "README.md"]
            ),
            "*"
        );
        eng.changelist_set(&wc, &paths(&["src/main.rs"]), Some("docs"), &ctx())
            .await
            .expect("changelist");
        let st = r.svn(&wc.root, &["status"]);
        assert!(st.contains("--- Changelist 'docs':"), "{st}");
        eng.ignore_add(
            &wc,
            &IgnoreAddRequest {
                wc_id: wc.id.clone(),
                pattern: "*.log".into(),
                dir: "src".into(),
                scope: IgnoreScope::SvnIgnore,
            },
            &ctx(),
        )
        .await
        .expect("ignore");
        assert_eq!(
            r.svn(&wc.root, &["propget", "--no-newline", "svn:ignore", "src"]),
            "*.log\n"
        );
        write(&wc.root, "src/x.log", "x\n");
        write(&wc.root, "junk.txt", "x\n");
        eng.cleanup(&wc, &cleanup_req(&wc, true, true), &ctx())
            .await
            .expect("cleanup");
        assert!(!wc.root.join("src/x.log").exists());
        assert!(!wc.root.join("junk.txt").exists());
        r.svn(&wc.root, &["cleanup"]);
        let mut lines = r.status(&wc);
        lines.retain(|l| !l.is_empty());
        assert_eq!(
            lines,
            vec![
                "        src/main.rs",
                " M      README.md",
                " M      src",
                "--- Changelist 'docs':",
            ],
            "sorted lines"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn conflict_resolved_with_theirs_matches_svn() {
        let Some(r) = Repo::new() else { return };
        let f = engine();
        let eng: &dyn EngineFacade = &f.engine;
        let a = r.checkout("conflict-a");
        let b = r.checkout("conflict-b");
        write(&a.root, "src/main.rs", "fn main() { a(); }\n");
        r.svn(&a.root, &["commit", "-q", "-m", "from a"]);
        write(&b.root, "src/main.rs", "fn main() { b(); }\n");
        r.svn(&b.root, &["update", "-q", "--accept", "postpone"]);
        assert!(
            r.status(&b)
                .iter()
                .any(|l| l.starts_with("C       src/main.rs")),
            "{:?}",
            r.status(&b)
        );
        eng.resolve(&b, &resolve_req(&b, ResolveChoice::Theirs), &ctx())
            .await
            .expect("resolve");
        assert!(r.status(&b).is_empty(), "{:?}", r.status(&b));
        assert_eq!(
            std::fs::read_to_string(b.root.join("src/main.rs")).unwrap(),
            "fn main() { a(); }\n"
        );
    }

    /// Checkout of trunk with a directory external `ext` and a file external `fext` fetched by
    /// svn, a separate checkout `nested`, local edits in all three and unversioned and ignored
    /// items.
    fn externals_wc(r: &Repo, name: &str) -> WcRef {
        let wc = r.checkout(name);
        let defs = r.base.join(format!("{name}-externals.txt"));
        std::fs::write(&defs, "^/trunk/src ext\n^/trunk/README.md fext\n").unwrap();
        r.svn(
            &wc.root,
            &[
                "propset",
                "-q",
                "svn:externals",
                "-F",
                defs.to_str().unwrap(),
                ".",
            ],
        );
        r.svn(&wc.root, &["commit", "-q", "-m", "externals"]);
        r.svn(&wc.root, &["update", "-q"]);
        r.svn(
            &wc.root,
            &["checkout", "-q", &format!("{}/trunk", r.url), "nested"],
        );
        write(&wc.root, "ext/main.rs", "edited in the external\n");
        write(
            &wc.root,
            "nested/README.md",
            "edited in the nested checkout\n",
        );
        write(&wc.root, "fext", "edited file external\n");
        write(&wc.root, "scratch.txt", "x\n");
        write(&wc.root, "debug.o", "x\n");
        wc
    }

    fn assert_externals_kept(r: &Repo, wc: &WcRef) {
        let st = r.status(wc);
        assert!(!st.iter().any(|l| l.contains("scratch.txt")), "{st:?}");
        assert_externals_intact(r, wc);
    }

    /// The external, the nested checkout and their local edits are as `externals_wc` made them.
    fn assert_externals_intact(r: &Repo, wc: &WcRef) {
        let st = r.status(wc);
        for line in ["X       ext", "M       ext/main.rs", "?       nested"] {
            assert!(st.iter().any(|l| l == line), "{line:?} in {st:?}");
        }
        assert_eq!(
            std::fs::read_to_string(wc.root.join("ext/main.rs")).unwrap(),
            "edited in the external\n"
        );
        assert_eq!(
            std::fs::read_to_string(wc.root.join("nested/README.md")).unwrap(),
            "edited in the nested checkout\n"
        );
        let nested = r.svn(&wc.root.join("nested"), &["status", "--ignore-externals"]);
        assert!(
            nested.lines().any(|l| l == "M       README.md"),
            "nested checkout intact: {nested}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cleanup_removing_unversioned_keeps_svn_externals_and_nested_checkouts() {
        let Some(r) = Repo::new() else { return };
        let f = engine();
        let eng: &dyn EngineFacade = &f.engine;
        let wc = externals_wc(&r, "ext-cleanup");
        eng.cleanup(&wc, &cleanup_req(&wc, true, true), &ctx())
            .await
            .expect("cleanup");
        assert_externals_kept(&r, &wc);
        assert!(!wc.root.join("debug.o").exists());
        assert!(
            r.status(&wc).iter().any(|l| l == "M   X   fext"),
            "file external kept: {:?}",
            r.status(&wc)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn revert_removing_unversioned_keeps_svn_externals_and_nested_checkouts() {
        let Some(r) = Repo::new() else { return };
        let f = engine();
        let eng: &dyn EngineFacade = &f.engine;
        let wc = externals_wc(&r, "ext-revert");
        eng.revert(&wc, &revert_req(&wc, &[""], Some(true)), &ctx())
            .await
            .expect("revert");
        assert_externals_kept(&r, &wc);
        assert!(wc.root.join("debug.o").exists(), "ignored items stay");
        assert!(wc.root.join("fext").exists(), "file external kept");
    }

    /// Revert with `remove_unversioned` of `rel`, which svn would revert inside the external or
    /// nested working copy, is refused and deletes nothing.
    async fn assert_svn_revert_refused(f: &Fixture, wc: &WcRef, rel: &str, kept: &str) {
        let before = std::fs::read(wc.root.join(kept)).unwrap();
        let err = (&f.engine as &dyn EngineFacade)
            .revert(wc, &revert_req(wc, &[rel], Some(true)), &ctx())
            .await
            .expect_err(rel);
        assert_eq!(err.code, ErrorCode::WcInvalidOperation, "{rel}");
        assert!(
            err.message.contains("open that working copy"),
            "{rel}: {}",
            err.message
        );
        assert_eq!(std::fs::read(wc.root.join(kept)).unwrap(), before, "{kept}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn revert_of_a_file_inside_an_svn_external_is_refused() {
        let Some(r) = Repo::new() else { return };
        let f = engine();
        let wc = externals_wc(&r, "in-ext");
        assert_svn_revert_refused(&f, &wc, "ext/main.rs", "ext/main.rs").await;
        assert_externals_intact(&r, &wc);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn revert_of_a_file_inside_a_nested_checkout_is_refused() {
        let Some(r) = Repo::new() else { return };
        let f = engine();
        let wc = externals_wc(&r, "in-nested");
        assert_svn_revert_refused(&f, &wc, "nested/README.md", "nested/README.md").await;
        assert_externals_intact(&r, &wc);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn revert_of_a_directory_inside_a_checkout_hidden_in_an_unversioned_directory_is_refused()
    {
        let Some(r) = Repo::new() else { return };
        let f = engine();
        let wc = r.checkout("in-hidden");
        std::fs::create_dir_all(wc.root.join("u")).unwrap();
        r.svn(
            &wc.root.join("u"),
            &["checkout", "-q", &format!("{}/trunk", r.url), "inner"],
        );
        write(
            &wc.root,
            "u/inner/src/main.rs",
            "edited in the hidden checkout\n",
        );
        assert_svn_revert_refused(&f, &wc, "u/inner/src", "u/inner/src/main.rs").await;
        let inner = r.svn(&wc.root.join("u/inner"), &["status"]);
        assert!(
            inner.lines().any(|l| l == "M       src/main.rs"),
            "hidden checkout intact: {inner}"
        );
    }

    trait TapSort {
        fn tap_sort(self) -> Self;
    }

    impl TapSort for Vec<String> {
        fn tap_sort(mut self) -> Self {
            self.sort();
            self
        }
    }
}
