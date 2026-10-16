//! `svn commit`: harvest, upload through a [`CommitEditor`], then fold the result into the
//! working copy.
//!
//! Ordering guarantees (see `journal.rs` for the crash story):
//!
//! * authentication happens before the working copy lock is taken;
//! * nothing in the working copy changes before the server merge succeeded;
//! * the journal records the server transaction before any content is sent, that the `MERGE`
//!   may be in flight right before it is sent, and the revision right after it;
//!   [`recover_commits`] / [`recover_wc_commits`] complete or clean up whatever a crash left;
//! * a `MERGE` whose outcome is unknown ([`ErrorCode::CommitOutcomeUnknown`]) keeps its journal
//!   and its transaction: nothing is aborted, the working copy is untouched, and no new commit
//!   of the working copy starts until recovery settled it from the repository;
//! * the post-commit update of all committed paths is one database transaction;
//! * after MERGE, cancellation is ignored and the commit is never reported as failed; a merged
//!   journal left by a crash is finalized before the next commit on the same working copy, and
//!   finalizing never moves a node's BASE back to an older revision.
//!
//! Failpoints (feature `failpoints`): `commit.after_txn_created` fires once the server
//! transaction exists, `commit.after_merge_sent_marker` once the journal says the `MERGE` may be
//! sent (an error vetoes the `MERGE`), `commit.journal_merged_write` instead of writing the
//! merged revision to the journal, `commit.after_merge_before_finalize` after MERGE, before the
//! working copy is touched, `commit.finalize` in place of the working copy update, and
//! `commit.after_finalize_before_queue` once the rows and the `file-commit` work items are
//! committed but before they ran; all turn the configured `return` action into an error.

/// Evaluates a failpoint that injects an `Err` into the enclosing `Result` function.
#[cfg(feature = "failpoints")]
macro_rules! failpoint {
    ($name:literal) => {
        fail::fail_point!($name, |_| Err(svn_types::SvnError::new(
            svn_types::ErrorCode::Internal,
            concat!("failpoint ", $name)
        )));
    };
}

#[cfg(not(feature = "failpoints"))]
macro_rules! failpoint {
    ($name:literal) => {};
}

pub mod collect;
pub mod drive;
pub mod file_commit;
pub mod finalize;
pub mod journal;
mod parallel;
pub mod recovery;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use svn_ra_api::{CommitObserver, RaSession, ReplayableSource};
use svn_types::{
    CancelToken, Checksum, Depth, ErrorCode, PropChange, RelPath, Result, Revnum, SvnError,
};
use svn_wc::WcDb;
use svn_wc::db::{LockState, ObtainMode};
use svn_wc::pristine::PristineIndex;
use svn_wc::workqueue::WorkItem;

use self::collect::{CollectOptions, Committable, ItemAction, collect};
use self::drive::{DriveContext, DriveItem, PreparedText, drive};
use self::journal::{JournalItem, JournalLock, JournalObserver, JournalStore, JournalText};
use self::recovery::Reconciled;
use crate::local::content::detranslate_to_file;
use crate::local::paths::{abs_of, rel_of};
use crate::wc::run_work_queue;

pub use self::recovery::{RecoveryReport, WcCommitRecovery, recover_commits, recover_wc_commits};

/// Parameters of [`commit`].
#[derive(Debug, Clone)]
pub struct CommitOptions {
    /// Log message.
    pub message: String,
    /// Depth below directory targets.
    pub depth: Depth,
    /// Keep the locks of committed files.
    pub keep_locks: bool,
    /// Keep changelist membership of committed files.
    pub keep_changelist: bool,
    /// Only commit files of these changelists (empty = all).
    pub changelists: Vec<String>,
    /// Revision properties to set besides the log message.
    pub rev_props: Vec<PropChange>,
    /// Repository path of the session URL (empty when the session is at the repository root);
    /// every committed path must lie below it.
    pub session_prefix: String,
    /// User name the server will record as the author, when known. Journaled so that recovery
    /// can recognize the commit in the log after an unknown `MERGE` outcome.
    pub author: Option<String>,
}

impl CommitOptions {
    /// Options for a plain recursive commit with `message`.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            depth: Depth::Infinity,
            keep_locks: false,
            keep_changelist: false,
            changelists: Vec::new(),
            rev_props: Vec::new(),
            session_prefix: String::new(),
            author: None,
        }
    }
}

/// Environment of a commit.
pub struct CommitContext<'a> {
    /// Repository session.
    pub ra: &'a dyn RaSession,
    /// Application state directory holding the commit journals.
    pub state_dir: &'a Path,
    /// Cancellation; a commit cancelled before MERGE leaves the working copy untouched. Once
    /// the server accepted the MERGE it is ignored: the commit is finished and reported.
    pub cancel: &'a CancelToken,
}

/// What a successful commit produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitResult {
    /// New revision.
    pub revision: Revnum,
    /// Post-commit hook warning, if any.
    pub post_commit_err: Option<String>,
    /// Working copy paths that were committed.
    pub committed: Vec<RelPath>,
    /// The commit is in the repository and the working copy rows are updated, but finishing the
    /// working files failed (e.g. a file is in use); `cleanup` completes it.
    pub wc_warning: Option<String>,
}

/// Reclassifies errors the RA layer could not map: out-of-date and lock failures keep their
/// Subversion code, so the app can offer "Update" or the lock dialog.
pub fn map_commit_error(mut e: SvnError) -> SvnError {
    if matches!(e.code, ErrorCode::Internal | ErrorCode::ServerUnsupported) {
        e.code = match e.svn_code {
            // FS_CONFLICT, FS_TXN_OUT_OF_DATE, WC_NOT_UP_TO_DATE, RA_OUT_OF_DATE, FS_OUT_OF_DATE;
            // like svn's `fixup_commit_error` also FS_NOT_FOUND / RA_DAV_PATH_NOT_FOUND (added
            // into a directory deleted on the server) and FS_ALREADY_EXISTS /
            // RA_DAV_ALREADY_EXISTS (added on the server too)
            Some(
                160_024 | 160_028 | 155_011 | 170_004 | 160_042 | 160_013 | 175_007 | 160_020
                | 175_005,
            ) => ErrorCode::OutOfDate,
            // FS_PATH_ALREADY_LOCKED, FS_NO_LOCK_TOKEN, FS_LOCK_OWNER_MISMATCH
            Some(160_035 | 160_038 | 160_039) => ErrorCode::LockedByOther,
            // FS_BAD_LOCK_TOKEN, FS_NO_SUCH_LOCK, FS_LOCK_EXPIRED
            Some(160_037 | 160_040 | 160_041) => ErrorCode::LockTokenInvalid,
            // REPOS_HOOK_FAILURE
            Some(165_001) => ErrorCode::HookFailed,
            _ => e.code,
        };
    }
    e
}

/// Longest common ancestor of two repository paths (`""` is the repository root).
fn common_ancestor(a: &str, b: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for (x, y) in a.split('/').zip(b.split('/')) {
        if x != y || x.is_empty() {
            break;
        }
        out.push(x);
    }
    out.join("/")
}

/// Parent of a repository path (`""` stays `""`).
fn repos_parent(p: &str) -> String {
    p.rsplit_once('/')
        .map_or_else(String::new, |(d, _)| d.to_owned())
}

/// Like svn's `svn_client__condense_commit_items`: the longest common ancestor of the items'
/// repository paths, moved to its parent while an item sits exactly there and is not a
/// directory with only property changes (the session root can only be opened, never added,
/// deleted or be a file). `items` must not be empty.
fn condensed_anchor(items: &[Committable]) -> String {
    let mut base = items
        .iter()
        .map(|it| it.repos_relpath.as_str())
        .reduce(common_ancestor_ref)
        .map(str::to_owned)
        .unwrap_or_default();
    while !base.is_empty()
        && items.iter().any(|it| {
            it.repos_relpath == base
                && !(it.kind == collect::ItemKind::Dir && it.action == ItemAction::Modify)
        })
    {
        base = repos_parent(&base);
    }
    base
}

/// [`common_ancestor`] as a prefix of `a`.
fn common_ancestor_ref<'a>(a: &'a str, b: &str) -> &'a str {
    let len = common_ancestor(a, b).len();
    &a[..len]
}

/// Whether repository path `path` is `anchor` or lies below it.
fn at_or_below(path: &str, anchor: &str) -> bool {
    anchor.is_empty()
        || path == anchor
        || path
            .strip_prefix(anchor)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The session anchor for committing `items` when the session is at `within`: `within` itself
/// while it can express every change (no reconnect for an ordinary commit), else svn's anchor,
/// condensed over the items only (a commit of changes below a switched subtree alone is
/// anchored at that subtree, not at its common ancestor with `within`).
fn anchor_for(items: &[Committable], within: &str) -> String {
    let anchor = condensed_anchor(items);
    if at_or_below(&anchor, within) {
        within.to_owned()
    } else {
        anchor
    }
}

/// Repository path (below the root) the commit session must be anchored at to commit the local
/// changes below `targets`: `within` (normally the working copy's own path) unless some change
/// lies outside it, e.g. below a switched subtree, or sits exactly at it in a way the session
/// root cannot express. `None` when there is nothing to commit. Read-only; [`commit`] harvests
/// again under the working copy lock and refuses, before any server transaction, changes that
/// fall outside [`CommitOptions::session_prefix`].
pub fn commit_anchor(
    db: &WcDb,
    targets: &[PathBuf],
    opts: &CommitOptions,
    within: &str,
) -> Result<Option<String>> {
    let rels: Vec<RelPath> = targets
        .iter()
        .map(|t| rel_of(db, t))
        .collect::<Result<_>>()?;
    let harvest = collect(
        db,
        &rels,
        &CollectOptions {
            depth: opts.depth,
            changelists: opts.changelists.clone(),
        },
    )?;
    if harvest.items.is_empty() {
        return Ok(None);
    }
    Ok(Some(anchor_for(&harvest.items, within)))
}

/// Commits the local changes below `targets` (absolute paths inside the working copy).
///
/// Returns `None` when there is nothing to commit. The handle must have a lock owner set.
pub async fn commit(
    db: &WcDb,
    ctx: &CommitContext<'_>,
    targets: &[PathBuf],
    opts: &CommitOptions,
) -> Result<Option<CommitResult>> {
    // No prompt may happen while the working copy is locked.
    ctx.ra.ensure_authenticated(ctx.cancel).await?;
    let root = RelPath::root();
    let reuse = db.wc_lock_held(&root)? == LockState::HeldByUs;
    if !reuse {
        db.wc_lock_obtain(&root, -1, ObtainMode::Cleanup)?;
    }
    let result = match if reuse {
        Ok(())
    } else {
        run_work_queue(db, ctx.cancel).map(|_| ())
    } {
        Ok(()) => commit_locked(db, ctx, targets, opts).await,
        Err(e) => Err(e),
    };
    if !reuse {
        let released = db.wc_lock_release(&root);
        if result.is_ok() {
            released?;
        }
    }
    result
}

async fn commit_locked(
    db: &WcDb,
    ctx: &CommitContext<'_>,
    targets: &[PathBuf],
    opts: &CommitOptions,
) -> Result<Option<CommitResult>> {
    let rels: Vec<RelPath> = targets
        .iter()
        .map(|t| rel_of(db, t))
        .collect::<Result<_>>()?;
    let store = JournalStore::new(ctx.state_dir);
    // A merged commit of this working copy that was never folded in (crash or failure after
    // MERGE) goes first, so the new commit starts from the true BASE.
    recovery::finalize_pending_for_wc(&store, db)?;
    let harvest = collect(
        db,
        &rels,
        &CollectOptions {
            depth: opts.depth,
            changelists: opts.changelists.clone(),
        },
    )?;
    if harvest.items.is_empty() {
        return Ok(None);
    }
    // Every change must be expressible below the session root (see `commit_anchor`); checked
    // before a server transaction exists.
    if !at_or_below(&condensed_anchor(&harvest.items), &opts.session_prefix) {
        let outside = harvest
            .items
            .iter()
            .find(|it| drive::editor_path(&opts.session_prefix, &it.repos_relpath).is_err())
            .map_or_else(
                || opts.session_prefix.clone(),
                |it| it.repos_relpath.clone(),
            );
        return Err(SvnError::new(
            ErrorCode::WcInvalidOperation,
            format!(
                "the change to '/{outside}' cannot be committed through a session at '/{}'; \
                 open the session at the common ancestor of the changes (see commit_anchor)",
                opts.session_prefix
            ),
        ));
    }
    ctx.cancel.check()?;

    // A revision made by this commit is newer than this: recovery searches the log from here
    // when the outcome of the MERGE is unknown.
    let start_youngest = ctx.ra.youngest_rev(ctx.cancel).await?;
    let id = store.new_id_for(db.wcroot());
    let data_dir = store.create_data_dir(&id)?;
    let prepared = match prepare_items(db, &harvest.items, &data_dir) {
        Ok(p) => p,
        Err(e) => {
            let _ = store.remove(&id);
            return Err(e);
        }
    };
    let journal_items: Vec<JournalItem> = prepared.iter().map(journal_item).collect();
    let journal_locks: Vec<JournalLock> = harvest
        .locks
        .iter()
        .map(|l| JournalLock {
            relpath: l.relpath.as_str().to_owned(),
            token: l.token.clone(),
        })
        .collect();
    let mut initial = store.begin(
        id.clone(),
        db.wcroot().to_path_buf(),
        opts.keep_locks,
        opts.keep_changelist,
        journal_items,
        journal_locks,
    );
    initial.start_youngest = Some(start_youngest.0);
    initial.message = Some(opts.message.clone());
    initial.author.clone_from(&opts.author);
    if let Err(e) = store.write(&initial) {
        let _ = store.remove(&id);
        return Err(e);
    }
    let observer = Arc::new(JournalObserver::new(store.clone(), initial));

    let tokens: Vec<(String, String)> = harvest
        .locks
        .iter()
        .filter_map(|l| {
            let path = drive::editor_path(&opts.session_prefix, &l.repos_relpath).ok()?;
            Some((path.as_str().to_owned(), l.token.clone()))
        })
        .collect();

    let editor = ctx
        .ra
        .commit_editor(
            &opts.message,
            &opts.rev_props,
            &tokens,
            opts.keep_locks,
            observer.clone(),
            ctx.cancel,
        )
        .await;
    let mut editor = match editor {
        Ok(e) => e,
        Err(e) => {
            // The RA layer deletes the transaction when it fails to hand out an editor; if the
            // journal names one anyway, recovery will delete it.
            if observer.snapshot().txn.is_none() {
                let _ = store.remove(&id);
            }
            return Err(map_commit_error(e));
        }
    };

    let driven = async {
        failpoint!("commit.after_txn_created");
        drive(
            editor.as_mut(),
            &prepared,
            &DriveContext {
                repos_root: ctx.ra.repos_root(),
                session_prefix: &opts.session_prefix,
                dir_revisions: &harvest.dir_revisions,
                cancel: ctx.cancel,
            },
        )
        .await
    }
    .await;
    if let Err(e) = driven {
        // Nothing was merged: delete the server transaction and keep the working copy as is.
        let abort = editor.abort_edit().await;
        if abort.is_ok() {
            let _ = store.remove(&id);
        }
        return Err(svn_ra_api::unsent_commit_error(map_commit_error(e), &abort));
    }

    let info = match editor.close_edit().await {
        Ok(info) => info,
        Err(e) => match after_failed_close(&store, &observer, ctx, e).await {
            AfterClose::Merged(info) => info,
            AfterClose::Failed(e) => return Err(e),
        },
    };

    failpoint!("commit.after_merge_before_finalize");

    // From here on the revision exists in the repository. Cancellation is ignored and nothing
    // is reported as a failed commit: the user must never retry a commit that went through.
    let journal = observer.snapshot();
    if let Err(e) = finalize_after_merge(db, &journal, &data_dir) {
        // The journal stays: commit recovery (or the next commit, once the journal records the
        // revision) finishes it.
        let mut err = SvnError::new(
            ErrorCode::WcNeedsCleanup,
            format!(
                "committed revision {}, but the working copy could not be updated yet; it is \
                 completed by commit recovery (cleanup) or the next commit: {}",
                info.revision.0, e.message
            ),
        );
        err.svn_code = e.svn_code;
        return Err(err);
    }
    failpoint!("commit.after_finalize_before_queue");
    // The rows and the `file-commit` items are committed in wc.db: the journal has done its
    // job, and any queued item left over is finished by the next operation or `cleanup`.
    let mut warnings = Vec::new();
    if let Err(e) = store.remove(&id) {
        warnings.push(format!(
            "could not remove the commit journal: {}",
            e.message
        ));
    }
    if let Err(e) = run_committed_queue(db) {
        warnings.push(format!(
            "updating the committed working files did not finish; run cleanup: {}",
            e.message
        ));
    }
    Ok(Some(CommitResult {
        revision: info.revision,
        post_commit_err: info.post_commit_err,
        committed: prepared.iter().map(|p| p.item.relpath.clone()).collect(),
        wc_warning: (!warnings.is_empty()).then(|| warnings.join("; ")),
    }))
}

/// `file-commit` items finished per transaction by [`run_committed_queue`].
const FILE_COMMIT_BATCH: usize = 1000;

/// Runs the work queue after [`finalize::finalize`], which queued one `file-commit` item per
/// committed file. Leading `file-commit` items are finished in batches: the fixups of a batch
/// run first, then ONE transaction stores their stat caches and deletes the items, the same
/// guarantee the runner gives per item (a crash before that transaction runs the batch again,
/// and the fixup is idempotent). Anything else, from the first other item on, goes through
/// the regular runner in queue order.
fn run_committed_queue(db: &WcDb) -> Result<usize> {
    let mut done = 0;
    let mut ids = Vec::new();
    let mut infos = Vec::new();
    for (id, skel) in db.work_items()? {
        let Ok(WorkItem::FileCommit { relpath }) = WorkItem::from_skel(&skel) else {
            break;
        };
        let path = RelPath::new(&relpath)?;
        if let Some(info) = file_commit::file_commit_fixup_deferred(db, &path)? {
            infos.push((path, info));
        }
        ids.push(id);
        if ids.len() >= FILE_COMMIT_BATCH {
            done += complete_file_commits(db, &mut ids, &mut infos)?;
        }
    }
    done += complete_file_commits(db, &mut ids, &mut infos)?;
    Ok(done + run_work_queue(db, &CancelToken::new())?)
}

fn complete_file_commits(
    db: &WcDb,
    ids: &mut Vec<i64>,
    infos: &mut Vec<(RelPath, (i64, i64))>,
) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    db.with_txn(|t| {
        for (path, (size, mtime)) in infos.iter() {
            match t.record_fileinfo(path, Some(*size), Some(*mtime)) {
                // The node went away meanwhile: the item is still done, like the runner's.
                Err(e) if e.code == ErrorCode::PathNotFound => {}
                other => other?,
            }
        }
        t.complete_work_items(ids)
    })?;
    let n = ids.len();
    ids.clear();
    infos.clear();
    Ok(n)
}

/// Folds the merged commit into the working copy (failpoint `commit.finalize` replaces it).
fn finalize_after_merge(db: &WcDb, journal: &journal::JournalData, data_dir: &Path) -> Result<()> {
    failpoint!("commit.finalize");
    finalize::finalize_fresh(db, journal, data_dir)
}

/// How a commit whose `close_edit` failed continues.
enum AfterClose {
    /// The `MERGE` went through (the failure was recording it, or recovery found the revision):
    /// finalize as usual.
    Merged(svn_ra_api::CommitInfo),
    /// The commit fails with this error; the journal was dropped or deliberately kept.
    Failed(SvnError),
}

/// Decides what a failed `close_edit` means for the journal. A journal whose `MERGE` may have
/// been sent is only dropped once the repository shows the commit did not happen.
async fn after_failed_close(
    store: &JournalStore,
    observer: &JournalObserver,
    ctx: &CommitContext<'_>,
    e: SvnError,
) -> AfterClose {
    let snap = observer.snapshot();
    if let Some(m) = &snap.merged {
        // MERGE succeeded but the observer failed to persist it: the in-memory journal has the
        // revision, so carry on and finalize. Try once more to put the revision on disk, so
        // recovery does not depend on finding the commit in the log.
        let info = svn_ra_api::CommitInfo {
            revision: Revnum(m.revision),
            date: m.date.map(svn_types::Timestamp),
            author: m.author.clone(),
            post_commit_err: None,
        };
        let _ = observer.on_merged(&info);
        return AfterClose::Merged(info);
    }
    if e.code == ErrorCode::CommitOutcomeUnknown {
        // The server may still apply the MERGE: neither the transaction nor the journal may go.
        return AfterClose::Failed(
            SvnError::new(
                ErrorCode::CommitOutcomeUnknown,
                format!(
                    "the commit may or may not have been applied (the server's answer to the \
                     final MERGE was lost); the working copy was not changed and the commit \
                     journal '{}' is kept; commit recovery settles it from the repository \
                     before the next commit of this working copy",
                    snap.id
                ),
            )
            .with_child(e),
        );
    }
    if !snap.merge_sent {
        // Failed before the MERGE was sent: nothing was merged. The RA layer deleted the
        // transaction (best effort); a journal that names one stays for recovery to make sure.
        if snap.txn.is_none() {
            let _ = store.remove(&snap.id);
        }
        return AfterClose::Failed(map_commit_error(e));
    }
    // The server refused the MERGE. That is how every ordinary failure (out of date, hooks,
    // locks) looks, but a storage failure after the revision was written can look the same, so
    // the repository decides before the journal goes.
    match recovery::reconcile(&snap, ctx.ra, ctx.cancel).await {
        Ok(Reconciled::NotCommitted | Reconciled::NoMatch(_)) => {
            let _ = store.remove(&snap.id);
            AfterClose::Failed(map_commit_error(e))
        }
        Ok(Reconciled::Committed(merge)) => {
            let revision = Revnum(merge.revision);
            let info = svn_ra_api::CommitInfo {
                revision,
                date: merge.date.map(svn_types::Timestamp),
                author: merge.author.clone(),
                post_commit_err: None,
            };
            let _ = observer.on_merged(&info);
            AfterClose::Merged(info)
        }
        Ok(
            Reconciled::Ambiguous(why)
            | Reconciled::NotSearched(why)
            | Reconciled::Unconfirmed(why),
        ) => AfterClose::Failed(
            SvnError::new(
                ErrorCode::WcNeedsCleanup,
                format!(
                    "the server refused the commit, but {why}; the commit journal '{}' is kept \
                     until this is resolved",
                    snap.id
                ),
            )
            .with_child(map_commit_error(e)),
        ),
        Err(check) => {
            let mut err = map_commit_error(e);
            err.message = format!(
                "{} (whether the commit was applied could not be confirmed: {}; the commit \
                 journal '{}' is kept for commit recovery)",
                err.message, check.message, snap.id
            );
            AfterClose::Failed(err)
        }
    }
}

fn journal_item(p: &DriveItem) -> JournalItem {
    JournalItem {
        relpath: p.item.relpath.as_str().to_owned(),
        repos_relpath: p.item.repos_relpath.clone(),
        kind: p.item.kind,
        action: p.item.action,
        text: p.text.as_ref().map(|t| t.record.clone()),
    }
}

/// Saves the repository-normal text of every changed file next to the journal and computes
/// what the editor needs to upload it.
fn prepare_items(
    db: &WcDb,
    items: &[collect::Committable],
    data_dir: &Path,
) -> Result<Vec<DriveItem>> {
    let out = items
        .iter()
        .enumerate()
        .map(|(n, item)| prepare_item(db, n, item, data_dir))
        .collect::<Result<Vec<_>>>()?;
    // The saved texts must survive a crash once the MERGE may be sent: recovery installs them
    // as the new pristines. One batch of syncs instead of one sync per file.
    let saved: Vec<PathBuf> = out
        .iter()
        .filter_map(|d| d.text.as_ref().map(|t| t.target.path.clone()))
        .collect();
    if !saved.is_empty() {
        parallel::sync_files(saved)?;
        parallel::sync_dir(data_dir)?;
    }
    Ok(out)
}

/// Saves the repository-normal text of a changed file as `<n>.txt` in `data_dir` (not synced
/// yet, see [`prepare_items`]) and says how to upload it.
fn prepare_item(
    db: &WcDb,
    n: usize,
    item: &collect::Committable,
    data_dir: &Path,
) -> Result<DriveItem> {
    let mut di = DriveItem {
        item: item.clone(),
        text: None,
    };
    if !item.needs_text || item.action == ItemAction::Delete {
        return Ok(di);
    }
    let store = db.pristine_store();
    let abs = abs_of(db, &item.relpath);
    let name = format!("{n}.txt");
    let dest = data_dir.join(&name);
    let digest = detranslate_to_file(db, &item.relpath, &abs, &dest)?;
    let (base, base_md5) = match &item.base_sha1 {
        Some(sha1) => match store.index().lookup(sha1).map_err(SvnError::from)? {
            Some(rec) if store.path_of(sha1).is_file() => (
                Some(ReplayableSource {
                    path: store.path_of(sha1),
                    translated: true,
                    size: Some(rec.size),
                }),
                Some(Checksum::Md5(rec.md5.0)),
            ),
            _ => (None, None),
        },
        None => (None, None),
    };
    di.text = Some(PreparedText {
        target: ReplayableSource {
            path: dest,
            translated: true,
            size: Some(digest.size),
        },
        base,
        base_md5,
        result_md5: Checksum::Md5(digest.md5.0),
        record: JournalText {
            sha1: digest.sha1.to_hex(),
            md5: digest.md5.to_hex(),
            size: digest.size,
            data: name,
        },
    });
    Ok(di)
}

#[cfg(test)]
mod anchor_tests {
    use super::collect::{Committable, ItemAction, ItemKind};
    use super::{anchor_for, common_ancestor, condensed_anchor};
    use svn_types::RelPath;

    fn item(repos: &str, kind: ItemKind, action: ItemAction) -> Committable {
        Committable {
            relpath: RelPath::root(),
            repos_relpath: repos.to_owned(),
            kind,
            action,
            copy_from: None,
            base_rev: Some(1),
            needs_text: false,
            base_sha1: None,
            prop_changes: Vec::new(),
            lock_token: None,
        }
    }

    #[test]
    fn common_ancestor_is_component_wise() {
        assert_eq!(common_ancestor("trunk/a", "trunk/ab"), "trunk");
        assert_eq!(common_ancestor("trunk", "branches/b"), "");
        assert_eq!(common_ancestor("", "trunk"), "");
        assert_eq!(common_ancestor("a/b/c", "a/b/c"), "a/b/c");
    }

    #[test]
    fn anchor_stays_at_the_working_copy_for_changes_below_it() {
        let items = [
            item("trunk/f", ItemKind::File, ItemAction::Modify),
            item("trunk/d/g", ItemKind::File, ItemAction::Add),
        ];
        assert_eq!(anchor_for(&items, "trunk"), "trunk");
        // Property changes of the working copy root itself are made on the session root.
        let root_props = [item("trunk", ItemKind::Dir, ItemAction::Modify)];
        assert_eq!(anchor_for(&root_props, "trunk"), "trunk");
    }

    #[test]
    fn anchor_moves_to_the_common_ancestor_of_switched_changes() {
        let items = [
            item("trunk/f", ItemKind::File, ItemAction::Modify),
            item("branches/b/x", ItemKind::File, ItemAction::Modify),
        ];
        assert_eq!(anchor_for(&items, "trunk"), "");
        let items = [
            item("proj/trunk/f", ItemKind::File, ItemAction::Modify),
            item("proj/branches/b/x", ItemKind::File, ItemAction::Delete),
        ];
        assert_eq!(anchor_for(&items, "proj/trunk"), "proj");
    }

    #[test]
    fn a_file_or_an_add_at_the_anchor_moves_it_to_the_parent() {
        let items = [item("trunk/sub/x", ItemKind::File, ItemAction::Modify)];
        assert_eq!(anchor_for(&items, "trunk/sub/x"), "trunk/sub");
        let items = [item("trunk/new", ItemKind::Dir, ItemAction::Add)];
        assert_eq!(anchor_for(&items, "trunk/new"), "trunk");
    }

    #[test]
    fn changes_outside_the_working_copy_alone_are_condensed_over_the_items_like_svn() {
        let items = [
            item("branches/b/x", ItemKind::File, ItemAction::Modify),
            item("branches/b/d/y", ItemKind::File, ItemAction::Delete),
        ];
        assert_eq!(condensed_anchor(&items), "branches/b");
        assert_eq!(anchor_for(&items, "trunk"), "branches/b");
        let items = [item("branches/b/x", ItemKind::File, ItemAction::Modify)];
        assert_eq!(anchor_for(&items, "trunk"), "branches/b");
        // Property changes of the switched folder itself are made on the session root.
        let items = [item("branches/b", ItemKind::Dir, ItemAction::Modify)];
        assert_eq!(anchor_for(&items, "trunk"), "branches/b");
        // A sibling of the working copy whose name extends it is outside it.
        let items = [item("trunk2/x", ItemKind::File, ItemAction::Modify)];
        assert_eq!(anchor_for(&items, "trunk"), "trunk2");
        // Changes deeper than the working copy keep the session where it is.
        let items = [item("trunk/a/b/c", ItemKind::File, ItemAction::Modify)];
        assert_eq!(condensed_anchor(&items), "trunk/a/b");
        assert_eq!(anchor_for(&items, "trunk"), "trunk");
        assert_eq!(anchor_for(&items, ""), "");
    }
}
