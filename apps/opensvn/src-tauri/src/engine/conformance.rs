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
    ChangeAction, CleanupRequest, CommitRequest, CopyLocationRequest, Depth, EntryKind, FileStatus,
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
    /// Deletes a file below a working copy root without telling the working copy (it becomes
    /// missing).
    fn remove_file(&self, root: &Path, rel: &str) -> Result<(), SvnError>;
    /// Real, empty, writable directory for files the engine writes outside a working copy.
    fn scratch_dir(&self) -> PathBuf;
    /// Leaves the lock situation of a crashed or foreign process on `dir` of a working copy,
    /// as it would be found when the working copy is opened.
    fn plant_lock(&self, root: &Path, dir: &str, owner: PlantedOwner) -> Result<(), SvnError>;
    /// The suite runs over a connection that breaks under requests on purpose (a test proxy
    /// that resets connections): a write may then fail the way a write whose connection broke
    /// must fail ([`broken_write`]), instead of succeeding. Default: no.
    fn writes_may_break(&self) -> bool {
        false
    }
    /// A position in the harness's record of what was sent (the proxy's request log), taken
    /// before each attempt of a write.
    fn write_mark(&self) -> usize {
        0
    }
    /// Checks a write that failed with `err` after `mark` against what a broken write must do,
    /// and panics otherwise: the connection really broke under a write, no write was sent
    /// twice, and nothing is left behind on the server (no transaction).
    fn check_broken_write(&self, mark: usize, err: &SvnError) {
        let _ = (mark, err);
    }
}

/// Whether `err` is how a write ends when its connection broke: never sent again (#166), it
/// fails with an unknown outcome (`CommitOutcomeUnknown`, `WriteOutcomeUnknown`), with the
/// connection failure itself, or, once settled, with an error that has one of those as cause.
pub fn broken_write(err: &SvnError) -> bool {
    let mut cur = Some(err);
    while let Some(e) = cur {
        if matches!(
            e.code,
            ErrorCode::CommitOutcomeUnknown | ErrorCode::WriteOutcomeUnknown
        ) || e.connection_failure().is_some()
        {
            return true;
        }
        cur = e.child.as_deref();
    }
    false
}

/// Whether a write that failed with `err` (sent after `mark`) ends the scenario: over a
/// connection that breaks on purpose ([`Harness::writes_may_break`]), a broken write
/// ([`broken_write`]) is the expected outcome once the harness checked it (the connection broke
/// under a write, nothing was sent twice, nothing is left on the server). Repeating it would
/// meet the same fault, so the steps after it are left to the runs without faults.
pub fn broken_write_ends_scenario(h: &dyn Harness, mark: usize, err: &SvnError) -> bool {
    if !h.writes_may_break() || !broken_write(err) {
        return false;
    }
    h.check_broken_write(mark, err);
    eprintln!(
        "ends as designed: the connection broke under a write, which was not sent again: {err}"
    );
    true
}

/// Runs the write `$op` (an engine call, without `.await`) and returns its result. Over a
/// connection that breaks on purpose, a checked broken write returns from the scenario
/// ([`broken_write_ends_scenario`]); otherwise this is `$op.await`.
#[macro_export]
macro_rules! write_op {
    ($h:expr, $op:expr) => {{
        let mark = $h.write_mark();
        match $op.await {
            Err(e) if $crate::engine::conformance::broken_write_ends_scenario($h, mark, &e) => {
                return;
            }
            result => result,
        }
    }};
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
        .expect("list")
        .entries;
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
    let new_rev = write_op!(
        h,
        e.commit(
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
    )
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
    write_op!(h, e.commit(&a, &commit_req(&a, "from a"), &ctx())).expect("commit a");

    // b is out of date: commit is refused
    let err =
        write_op!(h, e.commit(&b, &commit_req(&b, "from b"), &ctx())).expect_err("out of date");
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
    let err = write_op!(h, e.commit(&b, &commit_req(&b, "from b"), &ctx())).expect_err("conflict");
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
    write_op!(h, e.lock(&scope, Some("editing"), false, &ctx())).expect("lock");
    let st = status(h, &wc).await;
    let lock = find(&st, "README.md")
        .and_then(|s| s.lock.clone())
        .expect("lock in status");
    assert!(lock.mine);
    write_op!(h, e.unlock(&scope, false, &ctx())).expect("unlock");
    assert!(find(&status(h, &wc).await, "README.md").is_none_or(|s| s.lock.is_none()));
}

/// Server-side edits and copies.
pub async fn remote_edit_and_copy(h: &dyn Harness) {
    let e = h.engine();
    let url = h.repo_url();
    let head = e.repo_info(&url, &ctx()).await.unwrap().youngest_rev;
    let rev = write_op!(
        h,
        e.remote_edit(
            &RemoteEditPlan {
                ops: vec![EditOp::Mkdir {
                    url: format!("{url}/trunk/extra"),
                }],
                message: "Add extra".into(),
            },
            &ctx(),
        )
    )
    .expect("remote_edit");
    assert_eq!(rev.0, head.0 + 1);
    let copied = write_op!(
        h,
        e.copy_location(
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
    )
    .expect("copy");
    assert_eq!(copied.0, rev.0 + 1);
    let branches = e.branches_list(&url, &Layout::Auto, &ctx()).await.unwrap();
    assert!(
        branches.iter().any(|b| b.name == "conformance"),
        "{branches:?}"
    );
}

/// `repo_info_at` looks at the path in the peg revision: a folder deleted since is still found
/// there, and a revision past HEAD is an error.
pub async fn repo_info_at_a_peg_revision(h: &dyn Harness) {
    let e = h.engine();
    let url = h.repo_url();
    let before = e.repo_info(&url, &ctx()).await.unwrap().youngest_rev;
    let gone = format!("{url}/trunk/gone");
    write_op!(
        h,
        e.remote_edit(
            &RemoteEditPlan {
                ops: vec![EditOp::Mkdir { url: gone.clone() }],
                message: "Add gone".into(),
            },
            &ctx(),
        )
    )
    .expect("mkdir");
    let deleted = write_op!(
        h,
        e.remote_edit(
            &RemoteEditPlan {
                ops: vec![EditOp::Delete { url: gone.clone() }],
                message: "Delete gone".into(),
            },
            &ctx(),
        )
    )
    .expect("delete");
    let at_head = e
        .repo_info(&gone, &ctx())
        .await
        .expect_err("deleted at HEAD");
    assert_eq!(at_head.code, ErrorCode::PathNotFound, "{at_head:?}");
    let at_add = e
        .repo_info_at(&gone, Some(Rev(before.0 + 1)), &ctx())
        .await
        .expect("found at the peg revision");
    assert_eq!(at_add.kind, EntryKind::Dir);
    assert_eq!(at_add.youngest_rev, deleted, "youngest_rev stays HEAD");
    let before_add = e
        .repo_info_at(&gone, Some(before), &ctx())
        .await
        .expect_err("not there before it was added");
    assert_eq!(before_add.code, ErrorCode::PathNotFound, "{before_add:?}");
    e.repo_info_at(&url, Some(Rev(deleted.0 + 10)), &ctx())
        .await
        .expect_err("revision past HEAD");
    // `None` is HEAD, as `repo_info`.
    assert_eq!(
        e.repo_info_at(&url, None, &ctx()).await.unwrap(),
        e.repo_info(&url, &ctx()).await.unwrap()
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

    e.upgrade_format(&wc, 31, &ctx())
        .await
        .expect("upgrade_format to the current format");
}

/// Marking a move made outside the app, and changing the depth of a folder.
pub async fn mark_move_and_set_depth(h: &dyn Harness) {
    let e = h.engine();
    let (wc, _) = checkout(h, "mark-move", None).await;
    e.move_path(&wc, "README.md", "GUIDE.md", &ctx())
        .await
        .expect("move");
    e.mark_move(&wc, "README.md", "GUIDE.md", &ctx())
        .await
        .expect("mark_move");

    let (wc2, _) = checkout(h, "depth", None).await;
    e.set_depth(&wc2, "src", Depth::Empty, &ctx())
        .await
        .expect("set_depth");
}

/// Switch moves a working copy to another URL of the same repository.
pub async fn switch_to_a_branch(h: &dyn Harness) {
    let e = h.engine();
    let url = h.repo_url();
    write_op!(
        h,
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
    )
    .expect("copy");
    let (wc, _) = checkout(h, "switch", None).await;
    let reached = e
        .switch(
            &wc,
            &format!("{url}/branches/switch-target"),
            None,
            false,
            &ctx(),
        )
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

/// The trunk/branches/tags layout around a working copy URL is found through the engine, follows a
/// switch, and lists the branches to switch to. A location outside the layout has no branch.
pub async fn branch_detection_follows_a_switch(h: &dyn Harness) {
    use opensvn_ipc::BranchKind;

    use crate::layout::{EngineDirs, branch_from_url, detect};

    let e = h.engine();
    let url = h.repo_url();
    for to in ["branches/feature-y", "lib-copy"] {
        write_op!(
            h,
            e.copy_location(
                &CopyLocationRequest {
                    from_url: format!("{url}/trunk"),
                    from_rev: None,
                    to_url: format!("{url}/{to}"),
                    message: format!("Copy to {to}"),
                    switch_wc_id: None,
                    from_wc: None,
                },
                None,
                &ctx(),
            )
        )
        .expect("copy");
    }
    let (wc, info) = checkout(h, "branch-detect", None).await;
    let lister = EngineDirs(e);
    let found = detect(&lister, &info.repo_root_url, &info.url)
        .await
        .expect("detect");
    assert!(found.found);
    let current = found.current.expect("trunk is a branch");
    // The offline derivation used for the toolbar agrees with the server-side detection.
    assert_eq!(
        branch_from_url(&info.repo_root_url, &info.url, None),
        Some(current.clone())
    );
    assert_eq!(
        (current.kind, current.name.as_str()),
        (BranchKind::Trunk, "trunk")
    );

    let nodes = e
        .branches_list(&info.repo_root_url, &Layout::Auto, &ctx())
        .await
        .expect("branches");
    let target = nodes
        .iter()
        .find(|n| n.kind == BranchKind::Branch && n.name == "feature-y")
        .expect("listed branch");
    e.switch(&wc, &target.url, None, false, &ctx())
        .await
        .expect("switch");
    let info = e.wc_info(&wc.root, &ctx()).await.expect("info");
    let current = detect(&lister, &info.repo_root_url, &info.url)
        .await
        .expect("detect")
        .current
        .expect("branch after switch");
    assert_eq!(
        branch_from_url(&info.repo_root_url, &info.url, None),
        Some(current.clone())
    );
    assert_eq!(
        (current.kind, current.name.as_str()),
        (BranchKind::Branch, "feature-y")
    );
    assert_eq!(current.url, target.url);

    e.switch(&wc, &format!("{url}/lib-copy"), None, false, &ctx())
        .await
        .expect("switch outside the layout");
    let info = e.wc_info(&wc.root, &ctx()).await.expect("info");
    let outside = detect(&lister, &info.repo_root_url, &info.url)
        .await
        .expect("detect");
    assert!(outside.current.is_none(), "{:?}", outside.current);
    assert_eq!(branch_from_url(&info.repo_root_url, &info.url, None), None);
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

/// A locked file without local changes is listed in status. Committing another file keeps the
/// lock; committing the locked file releases it.
pub async fn locked_unmodified_file_in_status(h: &dyn Harness) {
    let e = h.engine();
    let (wc, _) = checkout(h, "locked-clean", None).await;
    let scope = LockScope::Wc {
        wc: wc.clone(),
        paths: vec!["README.md".into()],
    };
    write_op!(h, e.lock(&scope, Some("hold"), false, &ctx())).expect("lock");
    let lock_of = |st: &[FileStatus]| find(st, "README.md").and_then(|s| s.lock.clone());
    for query in [
        ALL,
        StatusQuery {
            include_ignored: false,
            include_unversioned: false,
        },
    ] {
        let st = e.status(&wc, query, &ctx()).await.expect("status");
        let readme = find(&st, "README.md").expect("locked unmodified file is listed");
        assert_eq!(readme.text, NodeStatus::Normal, "{readme:?}");
        let lock = readme.lock.clone().expect("lock in status");
        assert!(lock.mine);
        assert_eq!(lock.comment.as_deref(), Some("hold"));
        assert!(lock.token.is_none(), "lock tokens never leave the engine");
        assert_eq!(st.len(), 1, "nothing else is listed: {st:?}");
    }

    let commit = |path: &str| CommitRequest {
        wc_id: wc.id.clone(),
        paths: vec![path.into()],
        message: format!("change {path}"),
        keep_locks: false,
        auto_add_remove: false,
    };
    h.write_file(&wc.root, "src/main.rs", "fn main() { locked(); }\n")
        .unwrap();
    write_op!(h, e.commit(&wc, &commit("src/main.rs"), &ctx())).expect("commit another file");
    let st = status(h, &wc).await;
    assert!(
        lock_of(&st).is_some_and(|l| l.mine),
        "a commit of another file keeps the lock: {st:?}"
    );

    h.write_file(&wc.root, "README.md", "locked change\n")
        .unwrap();
    write_op!(h, e.commit(&wc, &commit("README.md"), &ctx())).expect("commit the locked file");
    assert!(
        status(h, &wc).await.is_empty(),
        "committing the locked file releases its lock"
    );
}

/// An unmodified `svn:needs-lock` file is listed (read-only until locked, doc 01 §10.4), with
/// and without its lock, and is never reported as a change.
pub async fn needs_lock_unmodified_file_in_status(h: &dyn Harness) {
    let e = h.engine();
    let (wc, _) = checkout(h, "needs-lock-clean", None).await;
    e.props_set(&wc, "README.md", "svn:needs-lock", Some("*"), &ctx())
        .await
        .expect("props_set");
    write_op!(
        h,
        e.commit(
            &wc,
            &CommitRequest {
                wc_id: wc.id.clone(),
                paths: vec!["README.md".into()],
                message: "needs lock".into(),
                keep_locks: false,
                auto_add_remove: false,
            },
            &ctx(),
        )
    )
    .expect("commit the property");
    let only_readme = |st: &[FileStatus]| {
        assert_eq!(st.len(), 1, "only the needs-lock file is listed: {st:?}");
        let f = find(st, "README.md")
            .expect("unmodified needs-lock file is listed")
            .clone();
        assert_eq!(f.text, NodeStatus::Normal, "{f:?}");
        assert_eq!(f.props, opensvn_ipc::PropStatus::Normal, "{f:?}");
        assert!(f.needs_lock, "{f:?}");
        f
    };
    for query in [
        ALL,
        StatusQuery {
            include_ignored: false,
            include_unversioned: false,
        },
    ] {
        let st = e.status(&wc, query, &ctx()).await.expect("status");
        assert!(only_readme(&st).lock.is_none());
    }

    let scope = LockScope::Wc {
        wc: wc.clone(),
        paths: vec!["README.md".into()],
    };
    write_op!(h, e.lock(&scope, Some("lock to edit"), false, &ctx())).expect("lock");
    let lock = only_readme(&status(h, &wc).await)
        .lock
        .expect("lock in status");
    assert!(lock.mine);
    write_op!(h, e.unlock(&scope, false, &ctx())).expect("unlock");
    assert!(only_readme(&status(h, &wc).await).lock.is_none());
}

/// A lock held elsewhere refuses a lock with `LockedByOther`; `lock_conflicts` names its owner,
/// comment and date (never the token), and only an explicit steal takes it over; a stale token
/// cannot unlock it, only an explicit break can.
pub async fn lock_held_elsewhere_and_steal(h: &dyn Harness) {
    let e = h.engine();
    let (holder, _) = checkout(h, "lock-holder", None).await;
    let (other, _) = checkout(h, "lock-other", None).await;
    let scope = |wc: &WcRef| LockScope::Wc {
        wc: wc.clone(),
        paths: vec!["README.md".into()],
    };
    write_op!(
        h,
        e.lock(&scope(&holder), Some("held elsewhere"), false, &ctx())
    )
    .expect("lock in the first working copy");
    assert!(
        e.lock_conflicts(&scope(&holder), &ctx())
            .await
            .expect("conflicts of the holder")
            .is_empty(),
        "the holder's own lock is not a conflict"
    );

    let err =
        write_op!(h, e.lock(&scope(&other), None, false, &ctx())).expect_err("locked elsewhere");
    assert_eq!(err.code, ErrorCode::LockedByOther, "{err}");
    let conflicts = e
        .lock_conflicts(&scope(&other), &ctx())
        .await
        .expect("conflicts");
    assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    let c = &conflicts[0];
    assert_eq!(c.path, "README.md");
    assert!(!c.owner.is_empty());
    assert_eq!(c.comment.as_deref(), Some("held elsewhere"));
    assert!(
        c.created_at.as_deref().is_some_and(|d| d.contains('T')),
        "{c:?}"
    );
    let url = format!("{}/trunk/README.md", h.repo_url());
    let by_url = e
        .lock_conflicts(&LockScope::Urls(vec![url.clone()]), &ctx())
        .await
        .expect("conflicts by URL");
    assert_eq!(by_url.len(), 1, "{by_url:?}");
    assert_eq!(by_url[0].path, url);
    assert!(
        find(&status(h, &other).await, "README.md").is_none(),
        "a refused lock records nothing"
    );

    write_op!(h, e.lock(&scope(&other), Some("stolen"), true, &ctx())).expect("steal");
    let lock = find(&status(h, &other).await, "README.md")
        .and_then(|s| s.lock.clone())
        .expect("stolen lock in status");
    assert!(lock.mine);
    assert!(
        e.lock_conflicts(&scope(&other), &ctx())
            .await
            .expect("conflicts after the steal")
            .is_empty()
    );

    // The first working copy's token is stale now: a plain unlock is refused, and only an
    // explicit break removes the lock held elsewhere.
    write_op!(h, e.unlock(&scope(&holder), false, &ctx()))
        .expect_err("a stolen lock cannot be released with the old token");
    assert_eq!(
        by_url_conflicts(h).await.len(),
        1,
        "the refused unlock kept the lock"
    );
    write_op!(h, e.unlock(&scope(&holder), true, &ctx())).expect("break the lock held elsewhere");
    assert!(by_url_conflicts(h).await.is_empty(), "the lock is broken");
    assert!(
        find(&status(h, &holder).await, "README.md").is_none_or(|s| s.lock.is_none()),
        "the breaking working copy forgets its old token"
    );
}

async fn by_url_conflicts(h: &dyn Harness) -> Vec<opensvn_ipc::LockConflict> {
    let url = format!("{}/trunk/README.md", h.repo_url());
    h.engine()
        .lock_conflicts(&LockScope::Urls(vec![url]), &ctx())
        .await
        .expect("conflicts by URL")
}

/// Commit with `auto_add_remove` adds selected unversioned files and deletes selected missing
/// ones in the same revision.
pub async fn commit_with_auto_add_remove(h: &dyn Harness) {
    let e = h.engine();
    let url = h.repo_url();
    let head = e.repo_info(&url, &ctx()).await.expect("info").youngest_rev;
    let (wc, _) = checkout(h, "auto-add-remove", None).await;
    h.write_file(&wc.root, "new.txt", "new\n").expect("write");
    h.remove_file(&wc.root, "README.md").expect("remove");
    let st = status(h, &wc).await;
    assert_eq!(find(&st, "new.txt").unwrap().text, NodeStatus::Unversioned);
    assert_eq!(find(&st, "README.md").unwrap().text, NodeStatus::Missing);

    let rev = write_op!(
        h,
        e.commit(
            &wc,
            &CommitRequest {
                wc_id: wc.id.clone(),
                paths: vec!["new.txt".into(), "README.md".into()],
                message: "Add new, drop readme".into(),
                keep_locks: false,
                auto_add_remove: true,
            },
            &ctx(),
        )
    )
    .expect("commit");
    assert_eq!(rev.0, head.0 + 1);
    assert!(status(h, &wc).await.is_empty(), "nothing left to commit");

    let names: Vec<_> = e
        .repo_list(&format!("{url}/trunk"), None, &ctx())
        .await
        .expect("list")
        .entries
        .into_iter()
        .map(|x| x.name)
        .collect();
    assert!(names.contains(&"new.txt".to_owned()), "{names:?}");
    assert!(!names.contains(&"README.md".to_owned()), "{names:?}");
    assert_eq!(
        e.repo_cat(&format!("{url}/trunk/new.txt"), None, &ctx())
            .await
            .expect("cat")
            .content,
        "new\n"
    );

    let log = e
        .log(
            &LogScope::Url(format!("{url}/trunk")),
            &LogGetRequest {
                target: LogTarget::Url {
                    url: format!("{url}/trunk"),
                },
                from_rev: None,
                to_rev: None,
                limit: 1,
                stop_on_copy: None,
                with_paths: true,
                filter: None,
            },
            &ctx(),
        )
        .await
        .expect("log");
    let mut changed: Vec<_> = log.entries[0]
        .changed_paths
        .clone()
        .expect("changed paths")
        .into_iter()
        .map(|c| (c.path, c.action))
        .collect();
    changed.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        changed,
        vec![
            ("/trunk/README.md".to_owned(), ChangeAction::D),
            ("/trunk/new.txt".to_owned(), ChangeAction::A),
        ]
    );
}

/// `svn update PATH` on a folder, a file inside a folder and a top-level file of a working copy
/// whose session is anchored at its root: only the named paths move to the new revision.
pub async fn update_of_sub_paths(h: &dyn Harness) {
    let e = h.engine();
    let (wc1, before) = checkout(h, "subpath-1", None).await;
    let (wc2, _) = checkout(h, "subpath-2", None).await;
    // Working file content through the engine (the fake engine keeps no files on disk).
    let read = |wc: &WcRef, rel: &str| {
        let (wc, rel) = (wc.clone(), rel.to_owned());
        async move {
            e.diff_wc(&wc, &rel, None, DiffOptions::default(), &ctx())
                .await
                .unwrap_or_else(|err| panic!("diff {rel}: {err}"))
                .right
                .unwrap_or_else(|| panic!("{rel} has no working content"))
                .content
        }
    };
    let commit = |paths: &[&str], message: &str| CommitRequest {
        wc_id: wc1.id.clone(),
        paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        message: message.into(),
        keep_locks: false,
        auto_add_remove: true,
    };
    let update = |paths: &[&str]| UpdateRequest {
        wc_id: wc2.id.clone(),
        paths: Some(paths.iter().map(|p| (*p).to_owned()).collect()),
        rev: None,
        depth: None,
    };

    h.write_file(&wc1.root, "README.md", "hello again\n")
        .expect("write");
    h.write_file(&wc1.root, "src/main.rs", "fn main() { run(); }\n")
        .expect("write");
    h.write_file(&wc1.root, "src/lib.rs", "pub fn run() {}\n")
        .expect("write");
    let r2 = write_op!(
        h,
        e.commit(
            &wc1,
            &commit(&["README.md", "src/main.rs", "src/lib.rs"], "r2"),
            &ctx(),
        )
    )
    .expect("commit r2");

    // A folder.
    let reached = e
        .update(&wc2, &update(&["src"]), &ctx())
        .await
        .expect("update src");
    assert_eq!(reached, r2);
    assert_eq!(read(&wc2, "src/main.rs").await, "fn main() { run(); }\n");
    assert_eq!(read(&wc2, "src/lib.rs").await, "pub fn run() {}\n");
    assert_eq!(
        read(&wc2, "README.md").await,
        "hello\n",
        "outside the path stays"
    );
    let info = e.wc_info(&wc2.root, &ctx()).await.expect("info");
    assert_eq!(
        info.rev_range,
        Some((before.base_rev, r2)),
        "mixed revisions"
    );

    // A file inside a folder, then a top-level file.
    h.write_file(&wc1.root, "src/main.rs", "fn main() { run(); run(); }\n")
        .expect("write");
    let r3 =
        write_op!(h, e.commit(&wc1, &commit(&["src/main.rs"], "r3"), &ctx())).expect("commit r3");
    let reached = e
        .update(&wc2, &update(&["src/main.rs"]), &ctx())
        .await
        .expect("update src/main.rs");
    assert_eq!(reached, r3);
    assert_eq!(
        read(&wc2, "src/main.rs").await,
        "fn main() { run(); run(); }\n"
    );
    e.update(&wc2, &update(&["README.md"]), &ctx())
        .await
        .expect("update README.md");
    assert_eq!(read(&wc2, "README.md").await, "hello again\n");
    assert!(status(h, &wc2).await.is_empty(), "nothing is left modified");
}

/// An update that merges an incoming change into a locally modified file conflicts: the update
/// itself succeeds and reaches the target revision, other incoming changes are applied, and the
/// conflicted file can be resolved to the local version and committed.
pub async fn update_ends_in_conflict(h: &dyn Harness) {
    let e = h.engine();
    let url = h.repo_url();
    let (a, _) = checkout(h, "update-conflict-a", None).await;
    let (b, _) = checkout(h, "update-conflict-b", None).await;
    h.write_file(&a.root, "README.md", "hello from a\n")
        .unwrap();
    h.write_file(&a.root, "src/main.rs", "fn main() { a(); }\n")
        .unwrap();
    let theirs = write_op!(
        h,
        e.commit(
            &a,
            &CommitRequest {
                wc_id: a.id.clone(),
                paths: vec!["README.md".into(), "src/main.rs".into()],
                message: "from a".into(),
                keep_locks: false,
                auto_add_remove: false,
            },
            &ctx(),
        )
    )
    .expect("commit a");

    h.write_file(&b.root, "README.md", "hello from b\n")
        .unwrap();
    let reached = e
        .update(
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
        .expect("an update that conflicts still succeeds");
    assert_eq!(reached, theirs);
    let info = e.wc_info(&b.root, &ctx()).await.expect("info");
    assert_eq!(info.base_rev, theirs);
    assert!(info.rev_range.is_none(), "{:?}", info.rev_range);

    let st = status(h, &b).await;
    assert_eq!(
        find(&st, "README.md").expect("conflicted").text,
        NodeStatus::Conflicted
    );
    assert!(
        find(&st, "src/main.rs").is_none(),
        "the other incoming change merged cleanly: {st:?}"
    );
    let main = e
        .diff_wc(&b, "src/main.rs", None, DiffOptions::default(), &ctx())
        .await
        .expect("diff");
    assert_eq!(main.right.expect("right").content, "fn main() { a(); }\n");

    e.resolve(
        &b,
        &ResolveRequest {
            wc_id: b.id.clone(),
            path: "README.md".into(),
            choice: ResolveChoice::Mine,
        },
        &ctx(),
    )
    .await
    .expect("resolve");
    let st = status(h, &b).await;
    assert_eq!(
        find(&st, "README.md").expect("still modified").text,
        NodeStatus::Modified,
        "{st:?}"
    );
    let rev = write_op!(
        h,
        e.commit(
            &b,
            &CommitRequest {
                wc_id: b.id.clone(),
                paths: vec!["README.md".into()],
                message: "keep b".into(),
                keep_locks: false,
                auto_add_remove: false,
            },
            &ctx(),
        )
    )
    .expect("commit after resolving");
    assert_eq!(rev.0, theirs.0 + 1);
    assert_eq!(
        e.repo_cat(&format!("{url}/trunk/README.md"), None, &ctx())
            .await
            .expect("cat")
            .content,
        "hello from b\n"
    );
}

/// Runs every scenario against fresh harnesses from `make`.
pub async fn run_all<H: Harness>(make: impl Fn() -> H) {
    repository_reads(&make()).await;
    commit_and_update_cycle(&make()).await;
    schedule_and_revert(&make()).await;
    conflict_blocks_commit_until_resolved(&make()).await;
    lock_and_unlock(&make()).await;
    remote_edit_and_copy(&make()).await;
    repo_info_at_a_peg_revision(&make()).await;
    cancellation_and_errors(&make()).await;
    working_copy_metadata_operations(&make()).await;
    mark_move_and_set_depth(&make()).await;
    switch_to_a_branch(&make()).await;
    branch_detection_follows_a_switch(&make()).await;
    repository_content_tools(&make()).await;
    open_recovers_dead_locks_and_reports_foreign_ones(&make()).await;
    locked_unmodified_file_in_status(&make()).await;
    needs_lock_unmodified_file_in_status(&make()).await;
    lock_held_elsewhere_and_steal(&make()).await;
    commit_with_auto_add_remove(&make()).await;
    update_ends_in_conflict(&make()).await;
    update_of_sub_paths(&make()).await;
}
