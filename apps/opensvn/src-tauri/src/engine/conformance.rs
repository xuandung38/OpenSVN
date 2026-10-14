//! Engine-independent behavior suite for [`EngineFacade`].
//!
//! Every implementation (the fake engine now, the real engine later) runs the same scenarios
//! through a [`Harness`] that supplies a repository in a known state and a way to change files
//! in a working copy. The scenarios use only the facade, so a behavior difference between the
//! fake and the real engine shows up as a failing scenario.
//!
//! Standard repository state the harness must provide (revision 1 or later, nothing else under
//! `trunk`): `trunk/README.md` = `hello\n`, `trunk/src/main.rs` = `fn main() {}\n`, and the
//! directories `branches` and `tags`.
//!
//! The scenarios panic on failure (they are test code).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use opensvn_ipc::{
    CleanupRequest, CommitRequest, CopyLocationRequest, Depth, EntryKind, FileStatus,
    IgnoreAddRequest, IgnoreScope, Layout, LogGetRequest, LogTarget, NodeStatus, ResolveChoice,
    ResolveRequest, Rev, RevertRequest, UpdateRequest, WcFormat, WcId,
};
use svn_types::{ErrorCode, SvnError};

use super::{
    CheckoutPlan, DiffOptions, EditOp, EngineFacade, LockScope, LogScope, OpContext,
    RemoteEditPlan, StatusQuery, WcInfo, WcRef,
};

/// What a scenario needs from the engine under test.
pub trait Harness {
    /// The engine under test.
    fn engine(&self) -> &dyn EngineFacade;
    /// Repository root URL (see the module docs for the required content).
    fn repo_url(&self) -> String;
    /// Empty absolute directory that scenarios may create working copies in.
    fn work_dir(&self) -> PathBuf;
    /// Creates or overwrites a file below a working copy root.
    fn write_file(&self, root: &Path, rel: &str, content: &str) -> Result<(), SvnError>;
    /// Real, empty, writable directory for files the engine writes outside a working copy.
    fn scratch_dir(&self) -> PathBuf;
    /// Leaves the lock situation of a crashed or foreign process on `dir` of a working copy,
    /// as it would be found when the working copy is opened.
    fn plant_lock(&self, root: &Path, dir: &str, owner: PlantedOwner) -> Result<(), SvnError>;
}

/// Who owns a planted lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlantedOwner {
    /// A process that no longer exists (recoverable without the user).
    DeadProcess,
    /// A live or unknown process (must be left alone and reported).
    Foreign,
}

const ALL: StatusQuery = StatusQuery {
    include_ignored: false,
    include_unversioned: true,
};

fn ctx() -> OpContext {
    OpContext::detached()
}

fn find<'a>(statuses: &'a [FileStatus], path: &str) -> Option<&'a FileStatus> {
    statuses.iter().find(|s| s.path == path)
}

async fn checkout(h: &dyn Harness, name: &str, rev: Option<Rev>) -> (WcRef, WcInfo) {
    let root = h.work_dir().join(name);
    let info = h
        .engine()
        .checkout(
            &CheckoutPlan {
                url: format!("{}/trunk", h.repo_url()),
                path: root.clone(),
                rev,
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
    let wc = WcRef {
        id: WcId(name.to_owned()),
        root: PathBuf::from(&info.root_path),
    };
    (wc, info)
}

async fn status(h: &dyn Harness, wc: &WcRef) -> Vec<FileStatus> {
    h.engine().status(wc, ALL, &ctx()).await.expect("status")
}

/// Reads, listing and log against the standard repository.
pub async fn repository_reads(h: &dyn Harness) {
    let e = h.engine();
    let url = h.repo_url();
    let info = e
        .repo_info(&format!("{url}/trunk"), &ctx())
        .await
        .expect("repo_info");
    assert_eq!(info.kind, EntryKind::Dir);
    assert!(info.youngest_rev.0 >= 1);
    assert!(!info.repo_uuid.is_empty());

    let entries = e
        .repo_list(&format!("{url}/trunk"), None, &ctx())
        .await
        .expect("list");
    let names: Vec<_> = entries.iter().map(|x| x.name.as_str()).collect();
    assert!(
        names.contains(&"README.md") && names.contains(&"src"),
        "{names:?}"
    );
    let first_file = entries
        .iter()
        .position(|x| x.kind == EntryKind::File)
        .unwrap();
    assert!(
        entries[..first_file]
            .iter()
            .all(|x| x.kind == EntryKind::Dir),
        "directories come first"
    );

    let text = e
        .repo_cat(&format!("{url}/trunk/README.md"), None, &ctx())
        .await
        .expect("cat");
    assert_eq!(text.content, "hello\n");

    let page = e
        .log(
            &LogScope::Url(format!("{url}/trunk")),
            &LogGetRequest {
                target: LogTarget::Url {
                    url: format!("{url}/trunk"),
                },
                from_rev: None,
                to_rev: None,
                limit: 100,
                stop_on_copy: None,
                with_paths: true,
                filter: None,
            },
            &ctx(),
        )
        .await
        .expect("log");
    assert!(!page.entries.is_empty());
    assert!(
        page.entries.windows(2).all(|w| w[0].rev > w[1].rev),
        "log is newest first"
    );

    let missing = e
        .repo_cat(&format!("{url}/trunk/no-such-file"), None, &ctx())
        .await
        .expect_err("missing file");
    assert_eq!(missing.code, ErrorCode::PathNotFound);

    let branches = e
        .branches_list(&url, &Layout::Auto, &ctx())
        .await
        .expect("branches");
    assert!(
        branches
            .iter()
            .any(|b| b.kind == opensvn_ipc::BranchKind::Trunk)
    );
    assert!(
        e.branches_list(&url, &Layout::None, &ctx())
            .await
            .expect("none")
            .is_empty()
    );
}

/// Checkout, edit, diff, commit, second checkout and update.
pub async fn commit_and_update_cycle(h: &dyn Harness) {
    let e = h.engine();
    let head = e
        .repo_info(&h.repo_url(), &ctx())
        .await
        .expect("info")
        .youngest_rev;
    let (wc1, info1) = checkout(h, "cycle-1", None).await;
    assert_eq!(info1.base_rev, head);
    assert!(info1.rev_range.is_none());
    assert!(status(h, &wc1).await.is_empty(), "fresh checkout is clean");

    h.write_file(&wc1.root, "README.md", "hello world\n")
        .expect("write");
    let st = status(h, &wc1).await;
    assert_eq!(
        find(&st, "README.md").expect("modified").text,
        NodeStatus::Modified
    );

    let diff = e
        .diff_wc(&wc1, "README.md", None, DiffOptions::default(), &ctx())
        .await
        .expect("diff");
    assert_eq!(diff.left.expect("left").content, "hello\n");
    assert_eq!(diff.right.expect("right").content, "hello world\n");

    let (wc2, _) = checkout(h, "cycle-2", None).await;
    let new_rev = e
        .commit(
            &wc1,
            &CommitRequest {
                wc_id: wc1.id.clone(),
                paths: vec!["README.md".into()],
                message: "Say hello to the world".into(),
                keep_locks: false,
                auto_add_remove: true,
            },
            &ctx(),
        )
        .await
        .expect("commit");
    assert_eq!(new_rev.0, head.0 + 1);
    assert!(
        status(h, &wc1).await.is_empty(),
        "committed change is gone from status"
    );

    let scan = e.status_remote(&wc2, &ctx()).await.expect("status_remote");
    assert_eq!(scan.incoming, 1);
    assert_eq!(scan.latest_rev, new_rev);
    assert!(
        find(&scan.statuses, "README.md")
            .is_some_and(|s| s.remote.as_ref().is_some_and(|r| r.out_of_date))
    );

    let reached = e
        .update(
            &wc2,
            &UpdateRequest {
                wc_id: wc2.id.clone(),
                paths: None,
                rev: None,
                depth: None,
            },
            &ctx(),
        )
        .await
        .expect("update");
    assert_eq!(reached, new_rev);
    assert_eq!(
        e.wc_info(&wc2.root, &ctx()).await.expect("info").base_rev,
        new_rev
    );
    assert!(status(h, &wc2).await.is_empty());
    let diff = e
        .diff_wc(
            &wc2,
            "README.md",
            Some(new_rev),
            DiffOptions::default(),
            &ctx(),
        )
        .await
        .expect("diff against head");
    assert_eq!(
        diff.left.expect("l").content,
        diff.right.expect("r").content
    );

    let log = e
        .log(
            &LogScope::Wc {
                wc: wc2.clone(),
                path: String::new(),
            },
            &LogGetRequest {
                target: LogTarget::Wc {
                    wc_id: wc2.id.clone(),
                    path: String::new(),
                },
                from_rev: None,
                to_rev: None,
                limit: 1,
                stop_on_copy: None,
                with_paths: false,
                filter: None,
            },
            &ctx(),
        )
        .await
        .expect("wc log");
    assert_eq!(log.entries[0].rev, new_rev);
    assert_eq!(log.entries[0].message, "Say hello to the world");
}

/// Add, delete and revert.
pub async fn schedule_and_revert(h: &dyn Harness) {
    let e = h.engine();
    let (wc, _) = checkout(h, "schedule", None).await;
    h.write_file(&wc.root, "new.txt", "new\n").expect("write");
    assert_eq!(
        find(&status(h, &wc).await, "new.txt").unwrap().text,
        NodeStatus::Unversioned
    );
    e.add_paths(&wc, &["new.txt".into()], &ctx())
        .await
        .expect("add");
    assert_eq!(
        find(&status(h, &wc).await, "new.txt").unwrap().text,
        NodeStatus::Added
    );
    e.revert(
        &wc,
        &RevertRequest {
            wc_id: wc.id.clone(),
            paths: vec!["new.txt".into()],
            remove_unversioned: None,
        },
        &ctx(),
    )
    .await
    .expect("revert add");
    assert_eq!(
        find(&status(h, &wc).await, "new.txt").unwrap().text,
        NodeStatus::Unversioned
    );

    e.delete_paths(&wc, &["README.md".into()], false, &ctx())
        .await
        .expect("delete");
    assert_eq!(
        find(&status(h, &wc).await, "README.md").unwrap().text,
        NodeStatus::Deleted
    );
    e.revert(
        &wc,
        &RevertRequest {
            wc_id: wc.id.clone(),
            paths: vec!["README.md".into()],
            remove_unversioned: None,
        },
        &ctx(),
    )
    .await
    .expect("revert delete");
    assert!(find(&status(h, &wc).await, "README.md").is_none());
}

/// Two working copies change the same line; the second update conflicts.
pub async fn conflict_blocks_commit_until_resolved(h: &dyn Harness) {
    let e = h.engine();
    let (a, _) = checkout(h, "conflict-a", None).await;
    let (b, _) = checkout(h, "conflict-b", None).await;
    let commit_req = |wc: &WcRef, msg: &str| CommitRequest {
        wc_id: wc.id.clone(),
        paths: vec!["src/main.rs".into()],
        message: msg.into(),
        keep_locks: false,
        auto_add_remove: true,
    };
    h.write_file(&a.root, "src/main.rs", "fn main() { a(); }\n")
        .unwrap();
    h.write_file(&b.root, "src/main.rs", "fn main() { b(); }\n")
        .unwrap();
    e.commit(&a, &commit_req(&a, "from a"), &ctx())
        .await
        .expect("commit a");

    // b is out of date: commit is refused
    let err = e
        .commit(&b, &commit_req(&b, "from b"), &ctx())
        .await
        .expect_err("out of date");
    assert_eq!(err.code, ErrorCode::OutOfDate);

    e.update(
        &b,
        &UpdateRequest {
            wc_id: b.id.clone(),
            paths: None,
            rev: None,
            depth: None,
        },
        &ctx(),
    )
    .await
    .expect("update b");
    assert_eq!(
        find(&status(h, &b).await, "src/main.rs").unwrap().text,
        NodeStatus::Conflicted
    );
    let err = e
        .commit(&b, &commit_req(&b, "from b"), &ctx())
        .await
        .expect_err("conflict");
    assert_eq!(err.code, ErrorCode::ConflictBlocksCommit);

    e.resolve(
        &b,
        &ResolveRequest {
            wc_id: b.id.clone(),
            path: "src/main.rs".into(),
            choice: ResolveChoice::Theirs,
        },
        &ctx(),
    )
    .await
    .expect("resolve");
    assert!(
        find(&status(h, &b).await, "src/main.rs").is_none(),
        "taking theirs leaves nothing to commit"
    );
}

/// Locks show up in status and can be released.
pub async fn lock_and_unlock(h: &dyn Harness) {
    let e = h.engine();
    let (wc, _) = checkout(h, "locks", None).await;
    let scope = LockScope::Wc {
        wc: wc.clone(),
        paths: vec!["README.md".into()],
    };
    e.lock(&scope, Some("editing"), false, &ctx())
        .await
        .expect("lock");
    let st = status(h, &wc).await;
    let lock = find(&st, "README.md")
        .and_then(|s| s.lock.clone())
        .expect("lock in status");
    assert!(lock.mine);
    e.unlock(&scope, false, &ctx()).await.expect("unlock");
    assert!(find(&status(h, &wc).await, "README.md").is_none_or(|s| s.lock.is_none()));
}

/// Server-side edits and copies.
pub async fn remote_edit_and_copy(h: &dyn Harness) {
    let e = h.engine();
    let url = h.repo_url();
    let head = e.repo_info(&url, &ctx()).await.unwrap().youngest_rev;
    let rev = e
        .remote_edit(
            &RemoteEditPlan {
                ops: vec![EditOp::Mkdir {
                    url: format!("{url}/trunk/extra"),
                }],
                message: "Add extra".into(),
            },
            &ctx(),
        )
        .await
        .expect("remote_edit");
    assert_eq!(rev.0, head.0 + 1);
    let copied = e
        .copy_location(
            &CopyLocationRequest {
                from_url: format!("{url}/trunk"),
                from_rev: None,
                to_url: format!("{url}/branches/conformance"),
                message: "Branch".into(),
                switch_wc_id: None,
                from_wc: None,
            },
            None,
            &ctx(),
        )
        .await
        .expect("copy");
    assert_eq!(copied.0, rev.0 + 1);
    let branches = e.branches_list(&url, &Layout::Auto, &ctx()).await.unwrap();
    assert!(
        branches.iter().any(|b| b.name == "conformance"),
        "{branches:?}"
    );
}

/// Cancellation and error mapping.
pub async fn cancellation_and_errors(h: &dyn Harness) {
    let e = h.engine();
    let root = h.work_dir().join("cancelled");
    let cancelled = OpContext::detached();
    cancelled.cancel.cancel();
    let err = e
        .checkout(
            &CheckoutPlan {
                url: format!("{}/trunk", h.repo_url()),
                path: root.clone(),
                rev: None,
                depth: Depth::Infinity,
                sparse: None,
                format: WcFormat::V31,
                store_pristine: true,
                ignore_externals: false,
            },
            &cancelled,
        )
        .await
        .expect_err("cancelled checkout");
    assert_eq!(err.code, ErrorCode::Cancelled);
    let not_wc = e
        .wc_info(&root, &ctx())
        .await
        .expect_err("nothing was created");
    assert_eq!(not_wc.code, ErrorCode::NotAWorkingCopy);
}

/// Properties, changelists, ignores, moves, depth, cleanup and format upgrade.
pub async fn working_copy_metadata_operations(h: &dyn Harness) {
    let e = h.engine();
    let (wc, _) = checkout(h, "metadata", None).await;
    let path = |p: &str| vec![p.to_owned()];

    e.props_set(&wc, "README.md", "svn:needs-lock", Some("*"), &ctx())
        .await
        .expect("props_set");
    let st = status(h, &wc).await;
    let readme = find(&st, "README.md").expect("property change is listed");
    assert!(readme.needs_lock);
    assert_eq!(readme.props, opensvn_ipc::PropStatus::Modified);
    e.props_set(&wc, "README.md", "svn:needs-lock", None, &ctx())
        .await
        .expect("props delete");
    assert!(find(&status(h, &wc).await, "README.md").is_none());

    e.changelist_set(&wc, &path("README.md"), Some("docs"), &ctx())
        .await
        .expect("changelist");
    assert_eq!(
        find(&status(h, &wc).await, "README.md")
            .and_then(|s| s.changelist.clone())
            .as_deref(),
        Some("docs")
    );
    e.changelist_set(&wc, &path("README.md"), None, &ctx())
        .await
        .expect("clear changelist");
    assert!(find(&status(h, &wc).await, "README.md").is_none());

    // A line break would smuggle a second pattern into `svn:ignore`: refused in both scopes.
    for scope in [IgnoreScope::SvnIgnore, IgnoreScope::Local] {
        for pattern in ["*.tmp\n*", "a\rb"] {
            e.ignore_add(
                &wc,
                &IgnoreAddRequest {
                    wc_id: wc.id.clone(),
                    pattern: pattern.into(),
                    dir: String::new(),
                    scope,
                },
                &ctx(),
            )
            .await
            .expect_err("a pattern with a line break");
        }
    }

    h.write_file(&wc.root, "build.log", "x\n").expect("write");
    e.ignore_add(
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
    .expect("ignore_add");
    assert!(
        find(&status(h, &wc).await, "build.log").is_none(),
        "ignored files are not listed"
    );

    e.move_path(&wc, "README.md", "GUIDE.md", &ctx())
        .await
        .expect("move");
    let st = status(h, &wc).await;
    assert_eq!(find(&st, "GUIDE.md").unwrap().text, NodeStatus::Added);
    assert_eq!(find(&st, "README.md").unwrap().text, NodeStatus::Deleted);
    e.mark_move(&wc, "README.md", "GUIDE.md", &ctx())
        .await
        .expect("mark_move");

    e.cleanup(
        &wc,
        &CleanupRequest {
            wc_id: wc.id.clone(),
            remove_unversioned: Some(true),
            remove_ignored: Some(true),
            vacuum_pristines: Some(false),
        },
        &ctx(),
    )
    .await
    .expect("cleanup");

    let (wc2, _) = checkout(h, "depth", None).await;
    e.set_depth(&wc2, "src", Depth::Empty, &ctx())
        .await
        .expect("set_depth");
    e.upgrade_format(&wc2, 31, &ctx())
        .await
        .expect("upgrade_format to the current format");
}

/// Switch moves a working copy to another URL of the same repository.
pub async fn switch_to_a_branch(h: &dyn Harness) {
    let e = h.engine();
    let url = h.repo_url();
    e.copy_location(
        &CopyLocationRequest {
            from_url: format!("{url}/trunk"),
            from_rev: None,
            to_url: format!("{url}/branches/switch-target"),
            message: "Branch for switch".into(),
            switch_wc_id: None,
            from_wc: None,
        },
        None,
        &ctx(),
    )
    .await
    .expect("copy");
    let (wc, _) = checkout(h, "switch", None).await;
    let reached = e
        .switch(&wc, &format!("{url}/branches/switch-target"), None, &ctx())
        .await
        .expect("switch");
    let info = e.wc_info(&wc.root, &ctx()).await.expect("info");
    assert_eq!(info.base_rev, reached);
    assert!(
        info.url.ends_with("/branches/switch-target"),
        "{}",
        info.url
    );
    assert!(status(h, &wc).await.is_empty());
}

/// Downloads, revision diffs and blame.
pub async fn repository_content_tools(h: &dyn Harness) {
    let e = h.engine();
    let url = h.repo_url();
    let out = h.scratch_dir().join("saved-readme.md");
    e.repo_save(&format!("{url}/trunk/README.md"), None, &out, &ctx())
        .await
        .expect("repo_save");
    assert_eq!(
        std::fs::read_to_string(&out).expect("saved file"),
        "hello\n"
    );

    let head = e.repo_info(&url, &ctx()).await.expect("info").youngest_rev;
    let diff = e
        .diff_rev(
            &format!("{url}/trunk/README.md"),
            Some(Rev(0)),
            Rev(1),
            DiffOptions::default(),
            &ctx(),
        )
        .await
        .expect("diff_rev");
    assert!(diff.left.is_none_or(|l| l.content.is_empty()));
    assert_eq!(diff.right.expect("right").content, "hello\n");
    assert!(head.0 >= 1);

    let lines = e
        .blame(&format!("{url}/trunk/README.md"), None, &ctx())
        .await
        .expect("blame");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].text, "hello");
    assert_eq!(lines[0].rev, Rev(1));
}

/// Opening a working copy recovers dead-owner locks and only reports foreign ones.
pub async fn open_recovers_dead_locks_and_reports_foreign_ones(h: &dyn Harness) {
    let e = h.engine();
    let (wc, _) = checkout(h, "recovery", None).await;
    let clean = e.open_wc(&wc.root, &ctx()).await.expect("open clean");
    assert!(clean.recovery.recovered.is_empty() && clean.recovery.needs_user_action.is_empty());

    h.plant_lock(&wc.root, "src", PlantedOwner::DeadProcess)
        .expect("plant dead");
    let opened = e.open_wc(&wc.root, &ctx()).await.expect("open after crash");
    assert_eq!(opened.recovery.recovered, vec!["src".to_owned()]);
    assert!(opened.recovery.needs_user_action.is_empty());
    let again = e.open_wc(&wc.root, &ctx()).await.expect("open again");
    assert!(again.recovery.recovered.is_empty(), "recovery happens once");

    h.plant_lock(&wc.root, "src", PlantedOwner::Foreign)
        .expect("plant foreign");
    let opened = e
        .open_wc(&wc.root, &ctx())
        .await
        .expect("open with foreign lock");
    assert!(
        opened.recovery.recovered.is_empty(),
        "a foreign lock is never taken over"
    );
    assert_eq!(opened.recovery.needs_user_action, vec!["src".to_owned()]);
    let again = e.open_wc(&wc.root, &ctx()).await.expect("open once more");
    assert_eq!(
        again.recovery.needs_user_action,
        vec!["src".to_owned()],
        "still waiting for the user"
    );

    let err = e
        .open_wc(&h.work_dir().join("not-a-wc"), &ctx())
        .await
        .expect_err("not a wc");
    assert_eq!(err.code, ErrorCode::NotAWorkingCopy);
}

/// Runs every scenario against fresh harnesses from `make`.
pub async fn run_all<H: Harness>(make: impl Fn() -> H) {
    repository_reads(&make()).await;
    commit_and_update_cycle(&make()).await;
    schedule_and_revert(&make()).await;
    conflict_blocks_commit_until_resolved(&make()).await;
    lock_and_unlock(&make()).await;
    remote_edit_and_copy(&make()).await;
    cancellation_and_errors(&make()).await;
    working_copy_metadata_operations(&make()).await;
    switch_to_a_branch(&make()).await;
    repository_content_tools(&make()).await;
    open_recovers_dead_locks_and_reports_foreign_ones(&make()).await;
}
