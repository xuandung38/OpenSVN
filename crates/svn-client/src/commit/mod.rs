//! `svn commit`: harvest, upload through a [`CommitEditor`], then fold the result into the
//! working copy.
//!
//! Ordering guarantees (see `journal.rs` for the crash story):
//!
//! * authentication happens before the working copy lock is taken;
//! * nothing in the working copy changes before the server merge succeeded;
//! * the journal records the server transaction before any content is sent and the revision
//!   right after MERGE; [`recover_commits`] completes or cleans up whatever a crash left;
//! * the post-commit update of all committed paths is one database transaction.
//!
//! Failpoints (feature `failpoints`): `commit.after_txn_created` fires once the server
//! transaction exists and `commit.after_merge_before_finalize` after MERGE, before the working
//! copy is touched; both turn the configured `return` action into an error.

pub mod collect;
pub mod drive;
pub mod file_commit;
pub mod finalize;
pub mod journal;
pub mod recovery;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use svn_ra_api::{RaSession, ReplayableSource};
use svn_types::{
    CancelToken, Checksum, Depth, ErrorCode, PropChange, RelPath, Result, Revnum, SvnError,
};
use svn_wc::WcDb;
use svn_wc::db::{LockState, ObtainMode};
use svn_wc::pristine::PristineIndex;

use self::collect::{CollectOptions, ItemAction, collect};
use self::drive::{DriveContext, DriveItem, PreparedText, drive};
use self::journal::{JournalItem, JournalObserver, JournalStore, JournalText};
use crate::local::content::{detranslate_to_file, stat_cache};
use crate::local::paths::{abs_of, rel_of};
use crate::wc::run_work_queue;

pub use self::recovery::{RecoveryReport, recover_commits};

/// Evaluates a failpoint that injects an `Err` into the enclosing `Result` function.
#[cfg(feature = "failpoints")]
macro_rules! failpoint {
    ($name:literal) => {
        fail::fail_point!($name, |_| Err(SvnError::new(
            ErrorCode::Internal,
            concat!("failpoint ", $name)
        )));
    };
}

#[cfg(not(feature = "failpoints"))]
macro_rules! failpoint {
    ($name:literal) => {};
}

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
        }
    }
}

/// Environment of a commit.
pub struct CommitContext<'a> {
    /// Repository session.
    pub ra: &'a dyn RaSession,
    /// Application state directory holding the commit journals.
    pub state_dir: &'a Path,
    /// Cancellation; a commit cancelled before MERGE leaves the working copy untouched.
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
}

/// Reclassifies errors the RA layer could not map: out-of-date and lock failures keep their
/// Subversion code, so the app can offer "Update" or the lock dialog.
pub fn map_commit_error(mut e: SvnError) -> SvnError {
    if matches!(e.code, ErrorCode::Internal | ErrorCode::ServerUnsupported) {
        e.code = match e.svn_code {
            // FS_CONFLICT, FS_TXN_OUT_OF_DATE, WC_NOT_UP_TO_DATE, RA_OUT_OF_DATE, FS_OUT_OF_DATE
            Some(160_024 | 160_028 | 155_011 | 170_004 | 160_042) => ErrorCode::OutOfDate,
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
    ctx.cancel.check()?;

    let store = JournalStore::new(ctx.state_dir);
    let id = store.new_id();
    let data_dir = store.create_data_dir(&id)?;
    let prepared = match prepare_items(db, &harvest.items, &data_dir) {
        Ok(p) => p,
        Err(e) => {
            let _ = store.remove(&id);
            return Err(e);
        }
    };
    let journal_items: Vec<JournalItem> = prepared.iter().map(journal_item).collect();
    let initial = store.begin(
        id.clone(),
        db.wcroot().to_path_buf(),
        opts.keep_locks,
        opts.keep_changelist,
        journal_items,
    );
    if let Err(e) = store.write(&initial) {
        let _ = store.remove(&id);
        return Err(e);
    }
    let observer = Arc::new(JournalObserver::new(store.clone(), initial));

    let tokens: Vec<(String, String)> = prepared
        .iter()
        .filter_map(|p| {
            let t = p.item.lock_token.clone()?;
            let path = drive::editor_path(&opts.session_prefix, &p.item.repos_relpath).ok()?;
            Some((path.as_str().to_owned(), t))
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
        if editor.abort_edit().await.is_ok() {
            let _ = store.remove(&id);
        }
        return Err(map_commit_error(e));
    }

    let closed = editor.close_edit().await;
    let info = match closed {
        Ok(info) => info,
        Err(e) => {
            // MERGE succeeded but the observer failed to persist it: the in-memory journal
            // still has the revision, so carry on and finalize.
            if observer.snapshot().merged.is_none() {
                let txn_known = observer.snapshot().txn.is_some();
                if !txn_known {
                    let _ = store.remove(&id);
                }
                return Err(map_commit_error(e));
            }
            svn_ra_api::CommitInfo {
                revision: Revnum(
                    observer
                        .snapshot()
                        .merged
                        .as_ref()
                        .map_or(0, |m| m.revision),
                ),
                date: None,
                author: None,
                post_commit_err: None,
            }
        }
    };

    failpoint!("commit.after_merge_before_finalize");

    let journal = observer.snapshot();
    finalize::finalize(db, &journal, &data_dir)?;
    run_work_queue(db, ctx.cancel)?;
    store.remove(&id)?;
    Ok(Some(CommitResult {
        revision: info.revision,
        post_commit_err: info.post_commit_err,
        committed: prepared.iter().map(|p| p.item.relpath.clone()).collect(),
    }))
}

fn journal_item(p: &DriveItem) -> JournalItem {
    JournalItem {
        relpath: p.item.relpath.as_str().to_owned(),
        repos_relpath: p.item.repos_relpath.clone(),
        kind: p.item.kind,
        action: p.item.action,
        text: p.text.as_ref().map(|t| t.record.clone()),
        lock_token: p.item.lock_token.clone(),
    }
}

/// Saves the repository-normal text of every changed file next to the journal and computes
/// what the editor needs to upload it.
fn prepare_items(
    db: &WcDb,
    items: &[collect::Committable],
    data_dir: &Path,
) -> Result<Vec<DriveItem>> {
    let store = db.pristine_store();
    let mut out = Vec::with_capacity(items.len());
    for (n, item) in items.iter().enumerate() {
        let mut di = DriveItem {
            item: item.clone(),
            text: None,
        };
        if item.needs_text && item.action != ItemAction::Delete {
            let abs = abs_of(db, &item.relpath);
            let stat = stat_cache(&abs);
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
                    stat,
                },
            });
        }
        out.push(di);
    }
    Ok(out)
}
