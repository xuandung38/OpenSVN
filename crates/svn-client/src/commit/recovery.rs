//! Crash recovery for commits: finishes or cleans up the journals left in the state directory.
//!
//! [`recover_commits`] processes every journal (application start, before new write locks are
//! taken); [`recover_wc_commits`] only those of one working copy (after a failed commit, or when
//! a working copy is opened).
//!
//! Per journal:
//!
//! * a revision is recorded: the working copy is finalized, then the journal is removed;
//! * the `MERGE` may have been sent but no revision is recorded (crash during the `MERGE`, an
//!   unknown outcome, or a failure while recording the revision): the journal is reconciled with
//!   the server by [`reconcile`]. Transaction deleted by that call: never merged, the journal is
//!   dropped. Transaction gone, or its `DELETE` failed with a 5xx or in transport (outcome
//!   unknown): the log after the journal's start revision is searched for the commit (author
//!   when known, log message and changed paths); exactly one match is finalized, anything else
//!   keeps the journal and reports it as needing the user;
//! * only a transaction is recorded: it is deleted on the server (a `DELETE` the server fails
//!   is tried again a few times, see [`delete_unmerged_txn`]) and the journal is dropped;
//! * nothing reached the server: the journal is dropped.
//!
//! A journal whose outcome is unknown is never dropped without that check, and no journal is
//! dropped because it could not be read: a file of an unknown format version stays untouched,
//! a corrupt one is moved aside under a new name, an unreadable one stays.
//!
//! Files that cannot be used (unknown format without a readable root, corrupt, unreadable) are
//! attributed by the owner hint in their name ([`JournalStore::may_belong_to`]): one rule for all
//! three. Such a file that may belong to a working copy blocks that copy's commits
//! ([`finalize_pending_for_wc`]) and is reported by its recovery ([`recover_wc_commits`]); a
//! corrupt one is moved aside there (never deleted) and reported once, after which it no longer
//! blocks. A file that carries another working copy's hint is left to that working copy.

use std::collections::BTreeSet;
use std::path::Path;

use svn_ra_api::{LogRequest, RaSession, TxnAbort};
use svn_types::{CancelToken, Checksum, ErrorCode, LogEntry, Result, Revnum, SvnError};
use svn_wc::WcDb;

use super::drive::editor_path;
use super::finalize::finalize;
use super::journal::{JournalData, JournalLoadError, JournalMerge, JournalStore};
use crate::local::lock::with_wc_lock;
use crate::update::repos_relpath_of;
use crate::wc::run_work_queue;

/// What [`recover_commits`] did.
#[derive(Debug, Default)]
pub struct RecoveryReport {
    /// Journals whose commit had been merged and was folded into the working copy.
    pub finalized: Vec<String>,
    /// Journals whose server transaction was deleted (or that never reached the server).
    pub aborted: Vec<String>,
    /// Journals whose commit may or may not exist in the repository and that the log did not
    /// settle; they stay in place and block new commits of their working copy until resolved.
    pub needs_user: Vec<(String, SvnError)>,
    /// Journals that could not be processed; they are left in place for the next attempt (a
    /// corrupt one is moved aside first).
    pub failed: Vec<(String, SvnError)>,
}

/// What [`recover_wc_commits`] did for one working copy.
#[derive(Debug, Default)]
pub struct WcCommitRecovery {
    /// Journals whose commit was merged and is now folded into the working copy.
    pub finalized: Vec<String>,
    /// Revision each finalized journal's commit became, by journal id (same order as
    /// `finalized`).
    pub finalized_revisions: Vec<(String, u64)>,
    /// Journals of commits that certainly did not happen; dropped (their transaction deleted).
    pub aborted: Vec<String>,
    /// Journals whose commit may or may not exist and that the log did not settle; kept.
    pub needs_user: Vec<(String, SvnError)>,
    /// Journals that could not be processed now (no session, network, I/O); kept.
    pub failed: Vec<(String, SvnError)>,
}

/// Processes all journals in `state_dir`. `open_wc` opens a working copy by root path with its
/// lock owner registered (see `WcDb::open_and_recover`).
pub async fn recover_commits(
    state_dir: &Path,
    ra: &dyn RaSession,
    cancel: &CancelToken,
    open_wc: &(dyn Fn(&Path) -> Result<WcDb> + Sync),
) -> Result<RecoveryReport> {
    let store = JournalStore::new(state_dir);
    let mut report = RecoveryReport::default();
    let mut authenticated = false;
    for path in store.list()? {
        let data = match JournalStore::load(&path) {
            Ok(d) => d,
            Err(e) => {
                report
                    .failed
                    .push((path.display().to_string(), load_failure(&store, &path, e)));
                continue;
            }
        };
        let id = data.id.clone();
        let outcome = process(
            &store,
            data,
            Wc::Opener(open_wc),
            Some(ra),
            cancel,
            &mut authenticated,
        )
        .await;
        match outcome {
            Ok(Outcome::Finalized(_)) => report.finalized.push(id),
            Ok(Outcome::Aborted) => report.aborted.push(id),
            Ok(Outcome::NeedsUser(e)) => report.needs_user.push((id, e)),
            Err(e) => report.failed.push((id, e)),
        }
    }
    Ok(report)
}

/// Processes the journals of the working copy `db` in `state_dir`. Other working copies'
/// journals are not touched, judged by their content or, for a file that cannot be used, by the
/// owner hint in its name. A file that may belong to this working copy and cannot be used is
/// reported: an unknown format (in `needs_user`, also when it names no root), an unreadable file
/// (in `failed`, kept) or a corrupt one (in `failed`, moved aside).
///
/// Without `ra` only what needs no server is done: merged commits are finalized, journals that
/// never reached the server are dropped, and the rest is reported in `failed` and kept. With a
/// session it must be authenticated or able to authenticate without a prompt while the working
/// copy is unlocked: it is used before the working copy lock is taken for finalizing. The
/// handle must have a lock owner set.
pub async fn recover_wc_commits(
    state_dir: &Path,
    db: &WcDb,
    ra: Option<&dyn RaSession>,
    cancel: &CancelToken,
) -> Result<WcCommitRecovery> {
    let store = JournalStore::new(state_dir);
    let mut report = WcCommitRecovery::default();
    let mut authenticated = false;
    for path in store.list()? {
        if !JournalStore::may_belong_to(&path, db.wcroot()) {
            continue;
        }
        let data = match JournalStore::load(&path) {
            Ok(d) if d.wcroot == db.wcroot() => d,
            Ok(_) => continue,
            Err(JournalLoadError::UnsupportedVersion { wcroot, error, .. }) => {
                // Without a readable root it may be this working copy's: it blocks its commits
                // (see `finalize_pending_for_wc`), so it is reported here too.
                if wcroot.as_deref().is_none_or(|w| w == db.wcroot()) {
                    report.needs_user.push((path.display().to_string(), error));
                }
                continue;
            }
            Err(e) => {
                report
                    .failed
                    .push((path.display().to_string(), load_failure(&store, &path, e)));
                continue;
            }
        };
        let id = data.id.clone();
        match process(&store, data, Wc::Given(db), ra, cancel, &mut authenticated).await {
            Ok(Outcome::Finalized(rev)) => {
                report.finalized_revisions.push((id.clone(), rev));
                report.finalized.push(id);
            }
            Ok(Outcome::Aborted) => report.aborted.push(id),
            Ok(Outcome::NeedsUser(e)) => report.needs_user.push((id, e)),
            Err(e) => report.failed.push((id, e)),
        }
    }
    Ok(report)
}

/// The error reported for a journal file that could not be loaded; a corrupt one is moved
/// aside first (never deleted).
fn load_failure(store: &JournalStore, path: &Path, e: JournalLoadError) -> SvnError {
    match e {
        JournalLoadError::Corrupt(err) => match store.quarantine(path) {
            Ok(aside) => SvnError::new(
                err.code,
                format!("{}; moved aside to '{}'", err.message, aside.display()),
            ),
            Err(q) => err.with_child(q),
        },
        other => other.into_error(),
    }
}

/// Finalizes every merged journal of the working copy `db` (whose lock the caller holds), so
/// a commit never starts from a BASE that misses an earlier merged revision.
///
/// Refuses (`WcNeedsCleanup`) while a journal of this working copy has an unknown outcome or a
/// format this build does not know, and while a journal file that may belong to it (by its owner
/// hint, or without one) cannot be used: unreadable, corrupt, or of an unknown format without a
/// readable root. Its commit may have had its `MERGE` sent, so committing again before it is
/// resolved could commit the same change twice or build on a BASE that misses a revision.
/// Recovery ([`recover_wc_commits`]) reports such files and sets corrupt ones aside, which lifts
/// the block for those. Journals of other working copies and journals that only name a
/// transaction are left to recovery.
pub(crate) fn finalize_pending_for_wc(store: &JournalStore, db: &WcDb) -> Result<()> {
    for path in store.list()? {
        if !JournalStore::may_belong_to(&path, db.wcroot()) {
            continue;
        }
        let data = match JournalStore::load(&path) {
            Ok(d) => d,
            Err(JournalLoadError::UnsupportedVersion { wcroot, error, .. }) => {
                if wcroot.as_deref().is_none_or(|w| w == db.wcroot()) {
                    return Err(error);
                }
                continue;
            }
            Err(JournalLoadError::Unreadable(e)) => {
                return Err(SvnError::new(
                    ErrorCode::WcNeedsCleanup,
                    format!(
                        "the commit journal '{}' cannot be read, so it is unknown whether an \
                         earlier commit of this working copy was applied; make the file readable \
                         (or move it away after checking the repository log) before committing",
                        path.display()
                    ),
                )
                .with_child(e));
            }
            Err(JournalLoadError::Corrupt(e)) => {
                return Err(SvnError::new(
                    ErrorCode::WcNeedsCleanup,
                    format!(
                        "the commit journal '{}' is damaged, so it is unknown whether an earlier \
                         commit of this working copy was applied; commit recovery (when the working \
                         copy is opened) sets the file aside; check the repository log before \
                         committing",
                        path.display()
                    ),
                )
                .with_child(e));
            }
        };
        if data.wcroot != db.wcroot() {
            continue;
        }
        if data.merged.is_some() {
            finalize(db, &data, &store.data_dir_of(&data.id))?;
            run_work_queue(db, &CancelToken::new())?;
            store.remove(&data.id)?;
        } else if data.outcome_unknown() {
            return Err(outcome_unknown_blocks(&data));
        }
    }
    Ok(())
}

fn outcome_unknown_blocks(data: &JournalData) -> SvnError {
    SvnError::new(
        ErrorCode::WcNeedsCleanup,
        format!(
            "an earlier commit of this working copy (journal '{}', transaction '{}') may or may \
             not have been applied in the repository; it has to be resolved by commit recovery \
             (cleanup) before the next commit",
            data.id,
            data.txn.as_deref().unwrap_or("?")
        ),
    )
}

/// Working copy of a journal: given, or opened by root path.
#[derive(Clone, Copy)]
enum Wc<'a> {
    Given(&'a WcDb),
    Opener(&'a (dyn Fn(&Path) -> Result<WcDb> + Sync)),
}

enum Outcome {
    /// Folded into the working copy; the revision the commit became.
    Finalized(u64),
    Aborted,
    NeedsUser(SvnError),
}

fn finalize_journal(
    store: &JournalStore,
    data: &JournalData,
    wc: Wc<'_>,
    cancel: &CancelToken,
) -> Result<()> {
    let opened;
    let db = match wc {
        Wc::Given(db) => db,
        Wc::Opener(open) => {
            opened = open(&data.wcroot)?;
            &opened
        }
    };
    let data_dir = store.data_dir_of(&data.id);
    with_wc_lock(db, cancel, || {
        finalize(db, data, &data_dir)?;
        run_work_queue(db, cancel)?;
        Ok(())
    })?;
    store.remove(&data.id)
}

fn needs_session(data: &JournalData) -> SvnError {
    SvnError::new(
        ErrorCode::WcNeedsCleanup,
        format!(
            "the interrupted commit '{}' needs a connection to the repository to be resolved",
            data.id
        ),
    )
}

async fn process(
    store: &JournalStore,
    mut data: JournalData,
    wc: Wc<'_>,
    ra: Option<&dyn RaSession>,
    cancel: &CancelToken,
    authenticated: &mut bool,
) -> Result<Outcome> {
    if let Some(rev) = data.merged.as_ref().map(|m| m.revision) {
        finalize_journal(store, &data, wc, cancel)?;
        return Ok(Outcome::Finalized(rev));
    }
    if data.outcome_unknown() {
        let ra = ra.ok_or_else(|| needs_session(&data))?;
        authenticate(ra, cancel, authenticated).await?;
        return match reconcile(&data, ra, cancel).await? {
            Reconciled::NotCommitted => {
                store.remove(&data.id)?;
                Ok(Outcome::Aborted)
            }
            Reconciled::Committed(merge) => {
                let rev = merge.revision;
                data.merged = Some(merge);
                store.write(&data)?;
                finalize_journal(store, &data, wc, cancel)?;
                Ok(Outcome::Finalized(rev))
            }
            Reconciled::NoMatch(why)
            | Reconciled::Ambiguous(why)
            | Reconciled::NotSearched(why)
            | Reconciled::Unconfirmed(why) => Ok(Outcome::NeedsUser(SvnError::new(
                ErrorCode::WcNeedsCleanup,
                format!(
                    "cannot tell whether the commit '{}' (transaction '{}') was applied: \
                         {why}; check the repository log, then update or revert the working \
                         copy and remove the journal",
                    data.id,
                    data.txn.as_deref().unwrap_or("?")
                ),
            ))),
        };
    }
    if let Some(txn) = &data.txn {
        let ra = ra.ok_or_else(|| needs_session(&data))?;
        authenticate(ra, cancel, authenticated).await?;
        delete_unmerged_txn(ra, txn, cancel, TXN_DELETE_RETRY).await?;
    }
    store.remove(&data.id)?;
    Ok(Outcome::Aborted)
}

/// How often, and how far apart, [`delete_unmerged_txn`] sends the `DELETE` of a transaction.
#[derive(Debug, Clone, Copy)]
struct TxnDeleteRetry {
    /// `DELETE`s sent at most (the first one included).
    attempts: u32,
    /// Wait before the second `DELETE`; doubled before each later one.
    first_delay: std::time::Duration,
}

/// 5 `DELETE`s, 200 ms apart and doubling: at most 3 s of waiting in all.
const TXN_DELETE_RETRY: TxnDeleteRetry = TxnDeleteRetry {
    attempts: 5,
    first_delay: std::time::Duration::from_millis(200),
};

/// Deletes the transaction of a journal whose `MERGE` was never sent.
///
/// A process killed during an upload can leave the server still finishing its last request
/// (a `PUT` into the transaction), and FSFS then fails the `DELETE` with a 500 ("Directory not
/// empty" on the transaction directory). That is transient, so a `DELETE` the server or the
/// connection failed ([`TxnAbort::Unknown`]) is sent again, at most `retry.attempts` times
/// with a doubling pause that cancellation cuts short. Deleted now, or gone already (a 404), is
/// done. Any other error stops at once, and running out of attempts is an error too: the
/// journal is then kept and reported, never dropped, and the next recovery tries again.
///
/// Sending the `DELETE` again does not break the rule that a write is never sent twice: a
/// `DELETE` of a transaction is idempotent (a second one finds it gone), and since no `MERGE`
/// was sent the transaction can never become a revision, so whatever a repeated `DELETE` finds
/// changes nothing about the commit. This is also why [`reconcile`] does not retry: there the
/// `MERGE` may have been sent, a 500 can mean the `DELETE` raced the commit's own purge, and a
/// later "deleted" would no longer prove the commit did not happen.
async fn delete_unmerged_txn(
    ra: &dyn RaSession,
    txn: &str,
    cancel: &CancelToken,
    retry: TxnDeleteRetry,
) -> Result<()> {
    let mut delay = retry.first_delay;
    let mut attempt = 1;
    loop {
        let why = match ra.abort_txn_checked(txn, cancel).await {
            Ok(TxnAbort::Deleted | TxnAbort::Gone) => return Ok(()),
            Ok(TxnAbort::Unknown(why)) => why,
            // A session that cannot tell the cases apart: the plain, idempotent delete.
            Err(e) if e.code == ErrorCode::ServerUnsupported => return ra.abort_txn(txn).await,
            Err(e) => return Err(e),
        };
        if attempt >= retry.attempts {
            return Err(SvnError::new(
                ErrorCode::WcNeedsCleanup,
                format!(
                    "the server could not delete the interrupted commit's transaction '{txn}' \
                     after {attempt} attempts ({why}); the commit was never applied, and the \
                     next recovery tries again"
                ),
            ));
        }
        tracing::debug!(txn, attempt, %why, "transaction DELETE failed; trying again");
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = cancel.cancelled() => cancel.check()?,
        }
        delay = delay.saturating_mul(2);
        attempt += 1;
    }
}

async fn authenticate(ra: &dyn RaSession, cancel: &CancelToken, done: &mut bool) -> Result<()> {
    if !*done {
        ra.ensure_authenticated(cancel).await?;
        *done = true;
    }
    Ok(())
}

/// What the repository says about a commit whose `MERGE` outcome is unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reconciled {
    /// The transaction still existed (deleted now), or no revision was made since the commit
    /// started: the commit did not happen.
    NotCommitted,
    /// Exactly one revision since the commit started matches it.
    Committed(JournalMerge),
    /// The transaction is gone and newer revisions exist, but none matches the commit (the log
    /// message may have been edited, authz may hide paths, or an administrator removed the
    /// transaction); the reason.
    NoMatch(String),
    /// Several revisions match; the reason.
    Ambiguous(String),
    /// The transaction is gone and more revisions than [`RECOVERY_LOG_SEARCH_LIMIT`] were made
    /// since the commit started: the log was not searched; the reason.
    NotSearched(String),
    /// The server failed to answer the `DELETE` of the transaction (a 5xx, or a broken
    /// connection), so the transaction may still exist, and the log does not show the commit
    /// exactly once (no match, or no revision since the commit started); the reason. Unlike
    /// [`Reconciled::NoMatch`] this never counts as "not committed": the journal is kept.
    Unconfirmed(String),
}

/// Most revisions the log search of [`reconcile`] looks at. A journal settled long after the
/// crash on a busy repository would otherwise stream an unbounded log; beyond this the journal
/// is kept and reported for the user.
pub const RECOVERY_LOG_SEARCH_LIMIT: u64 = 1000;

/// Settles a journal whose `MERGE` may have been sent: deletes the transaction if it still
/// exists (then it was never merged), otherwise searches the revisions made since the commit
/// started for the commit: same author (when the journal knows it), same log message, same set
/// of changed paths, and the same text (MD5) for every file whose text was uploaded.
///
/// A `DELETE` the server fails with a 5xx (svn 1.15 FSFS answers 500 "Transaction cleanup
/// failed" when it races the commit's own purge after the revision was written) or that breaks
/// in transport proves nothing: the log is searched as for a transaction that is gone, exactly
/// one match settles the commit, and anything else is [`Reconciled::Unconfirmed`] (the
/// transaction may still exist, so it is never taken as "not committed").
///
/// A deletion that races a `MERGE` still running on the server is possible in theory (FSFS
/// purges the transaction after the revision is written); it is the same window
/// libsvn_ra_serf leaves when it deletes a transaction after a failed `MERGE`.
pub(crate) async fn reconcile(
    data: &JournalData,
    ra: &dyn RaSession,
    cancel: &CancelToken,
) -> Result<Reconciled> {
    reconcile_within(data, ra, cancel, RECOVERY_LOG_SEARCH_LIMIT).await
}

/// [`reconcile`] searching at most `max_revisions` revisions of the log.
async fn reconcile_within(
    data: &JournalData,
    ra: &dyn RaSession,
    cancel: &CancelToken,
    max_revisions: u64,
) -> Result<Reconciled> {
    let Some(txn) = &data.txn else {
        return Ok(Reconciled::NoMatch(
            "the journal names no transaction".to_owned(),
        ));
    };
    let state = match ra.abort_txn_checked(txn, cancel).await? {
        TxnAbort::Deleted => return Ok(Reconciled::NotCommitted),
        TxnAbort::Gone => TxnState::Gone,
        TxnAbort::Unknown(why) => TxnState::Unknown(why),
    };
    let found = search_log(data, ra, cancel, max_revisions, &state).await?;
    Ok(match (&state, found) {
        (TxnState::Unknown(_), Reconciled::NotCommitted) => Reconciled::Unconfirmed(format!(
            "{} and no revision was made since the commit started",
            state.describe()
        )),
        (TxnState::Unknown(_), Reconciled::NoMatch(why)) => Reconciled::Unconfirmed(why),
        (_, found) => found,
    })
}

/// What the `DELETE` of [`reconcile`] said about a transaction it did not delete.
enum TxnState {
    /// The server has no such transaction.
    Gone,
    /// The `DELETE` failed (5xx or transport); the reason.
    Unknown(String),
}

impl TxnState {
    fn describe(&self) -> String {
        match self {
            Self::Gone => "the transaction is gone".to_owned(),
            Self::Unknown(why) => {
                format!("the server could not tell whether the transaction still exists ({why})")
            }
        }
    }
}

/// The log part of [`reconcile_within`], for a transaction the `DELETE` did not prove unmerged.
async fn search_log(
    data: &JournalData,
    ra: &dyn RaSession,
    cancel: &CancelToken,
    max_revisions: u64,
    state: &TxnState,
) -> Result<Reconciled> {
    let Some(start) = data.start_youngest else {
        return Ok(Reconciled::NoMatch(
            "the journal does not record the revision the commit started from".to_owned(),
        ));
    };
    let head = ra.youngest_rev(cancel).await?.0;
    if head <= start {
        return Ok(Reconciled::NotCommitted);
    }
    if head - start > max_revisions {
        return Ok(Reconciled::NotSearched(format!(
            "{} and {} revisions (r{}..r{head}) were made since the commit started, more than \
             the {max_revisions} the automatic search covers",
            state.describe(),
            head - start,
            start + 1
        )));
    }
    let Some(message) = data.message.as_deref() else {
        return Ok(Reconciled::NoMatch(
            "the journal does not record the log message".to_owned(),
        ));
    };
    let wanted: BTreeSet<&str> = data
        .items
        .iter()
        .map(|i| i.repos_relpath.trim_matches('/'))
        .collect();
    let mut candidates: Vec<LogEntry> = Vec::new();
    let req = LogRequest {
        paths: Vec::new(),
        start: Some(Revnum(head)),
        end: Some(Revnum(start + 1)),
        limit: u32::try_from(max_revisions).ok(),
        discover_changed_paths: true,
        strict_node_history: false,
    };
    ra.log(
        &req,
        &mut |entry: LogEntry| {
            let same_author = data
                .author
                .as_deref()
                .is_none_or(|a| entry.author.as_deref() == Some(a));
            let paths: BTreeSet<&str> = entry
                .changed_paths
                .iter()
                .map(|c| c.path.as_str().trim_matches('/'))
                .collect();
            if entry.rev.0 > start
                && same_author
                && entry.message.as_deref() == Some(message)
                && paths == wanted
            {
                candidates.push(entry);
            }
            Ok(())
        },
        cancel,
    )
    .await?;
    let mut found = Vec::new();
    for entry in candidates {
        if same_texts(data, ra, entry.rev, cancel).await? {
            found.push(entry);
        }
    }
    Ok(match found.as_slice() {
        [one] => Reconciled::Committed(JournalMerge {
            revision: one.rev.0,
            date: one.date.map(|d| d.0),
            author: one.author.clone(),
        }),
        [] => Reconciled::NoMatch(format!(
            "{} but none of r{}..r{head} matches the commit",
            state.describe(),
            start + 1
        )),
        many => Reconciled::Ambiguous(format!(
            "{} revisions match the commit ({})",
            many.len(),
            many.iter()
                .map(|e| format!("r{}", e.rev.0))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    })
}

/// Every uploaded text of the journal has the same MD5 in revision `rev` (files outside the
/// session cannot be checked and are skipped).
async fn same_texts(
    data: &JournalData,
    ra: &dyn RaSession,
    rev: Revnum,
    cancel: &CancelToken,
) -> Result<bool> {
    let prefix = repos_relpath_of(ra.repos_root(), ra.session_url())?;
    for item in &data.items {
        let Some(text) = &item.text else { continue };
        let Ok(path) = editor_path(&prefix, &item.repos_relpath) else {
            continue;
        };
        let meta = match ra
            .get_file(&path, Some(rev), &mut tokio::io::sink(), cancel)
            .await
        {
            Ok(m) => m,
            Err(e) if e.code == ErrorCode::PathNotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        let Checksum::Md5(md5) = meta.md5 else {
            return Ok(false);
        };
        let hex: String = md5.iter().map(|b| format!("{b:02x}")).collect();
        if !hex.eq_ignore_ascii_case(&text.md5) {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use bytes::Bytes;
    use svn_ra_api::fake::{FakeChange, FakeRaSession};
    use svn_types::RelPath;

    use super::*;

    /// A journal whose transaction is gone, started at r0, committing `f` with message "m".
    fn gone_journal() -> JournalData {
        let store = JournalStore::new("/nowhere");
        let mut data = store.begin(
            "1".to_owned(),
            "/wc".into(),
            false,
            false,
            Vec::new(),
            Vec::new(),
        );
        data.txn = Some("gone".to_owned());
        data.merge_sent = true;
        data.start_youngest = Some(0);
        data.message = Some("m".to_owned());
        data
    }

    #[tokio::test]
    async fn the_log_search_is_bounded() {
        let ra = FakeRaSession::new("https://svn.test.local/repo", "uuid").expect("fake");
        for n in 0..3 {
            let path = RelPath::new(&format!("f{n}")).expect("path");
            ra.seed(
                "other",
                "x",
                &[FakeChange::PutFile(path, Bytes::from_static(b"x\n"))],
            )
            .expect("seed");
        }
        let data = gone_journal();
        let cancel = CancelToken::new();
        match reconcile_within(&data, &ra, &cancel, 2)
            .await
            .expect("reconcile")
        {
            Reconciled::NotSearched(why) => assert!(why.contains("r1..r3"), "{why}"),
            other => panic!("expected the search to be skipped, got {other:?}"),
        }
        // Within the bound the log is searched (and nothing matches).
        assert!(matches!(
            reconcile_within(&data, &ra, &cancel, 3)
                .await
                .expect("reconcile"),
            Reconciled::NoMatch(_)
        ));
        assert_eq!(RECOVERY_LOG_SEARCH_LIMIT, 1000);
    }

    struct NoObserver;

    impl svn_ra_api::CommitObserver for NoObserver {
        fn on_txn_created(&self, _txn: &str) -> Result<()> {
            Ok(())
        }
        fn on_merged(&self, _info: &svn_ra_api::CommitInfo) -> Result<()> {
            Ok(())
        }
    }

    /// A session with one open transaction and its name.
    async fn open_txn() -> (FakeRaSession, String) {
        let ra = FakeRaSession::new("https://svn.test.local/repo", "uuid").expect("fake");
        let editor = ra
            .commit_editor(
                "m",
                &[],
                &[],
                false,
                std::sync::Arc::new(NoObserver),
                &CancelToken::new(),
            )
            .await
            .expect("editor");
        std::mem::forget(editor);
        let txn = ra.open_txns().pop().expect("txn");
        (ra, txn)
    }

    const FAST: TxnDeleteRetry = TxnDeleteRetry {
        attempts: 5,
        first_delay: std::time::Duration::from_millis(1),
    };

    #[tokio::test]
    async fn a_transient_txn_delete_failure_is_retried() {
        let (ra, txn) = open_txn().await;
        ra.fail_txn_deletes_times("transactions/1-1.txn: Directory not empty", 4);
        delete_unmerged_txn(&ra, &txn, &CancelToken::new(), FAST)
            .await
            .expect("the fifth DELETE gets through");
        assert!(ra.open_txns().is_empty());
    }

    #[tokio::test]
    async fn the_txn_delete_retries_are_bounded() {
        let (ra, txn) = open_txn().await;
        ra.fail_txn_deletes_times("Directory not empty", 5);
        let err = delete_unmerged_txn(&ra, &txn, &CancelToken::new(), FAST)
            .await
            .expect_err("five failures");
        assert_eq!(err.code, ErrorCode::WcNeedsCleanup, "{err}");
        assert!(err.message.contains("after 5 attempts"), "{err}");
        assert!(err.message.contains("Directory not empty"), "{err}");
        assert_eq!(ra.open_txns(), vec![txn.clone()], "transaction kept");
        // All five faults were used: the next DELETE is answered normally.
        delete_unmerged_txn(&ra, &txn, &CancelToken::new(), FAST)
            .await
            .expect("deleted");
        assert!(ra.open_txns().is_empty());
    }

    #[tokio::test]
    async fn a_gone_txn_needs_no_retry() {
        let (ra, txn) = open_txn().await;
        ra.abort_txn(&txn).await.expect("admin removes it");
        delete_unmerged_txn(&ra, &txn, &CancelToken::new(), FAST)
            .await
            .expect("gone is done");
    }

    #[tokio::test]
    async fn cancellation_cuts_the_retry_pause_short() {
        let (ra, txn) = open_txn().await;
        ra.fail_txn_deletes(Some("Directory not empty"));
        let cancel = CancelToken::new();
        let slow = TxnDeleteRetry {
            attempts: 5,
            first_delay: std::time::Duration::from_secs(60),
        };
        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            canceller.cancel();
        });
        let started = std::time::Instant::now();
        let err = delete_unmerged_txn(&ra, &txn, &cancel, slow)
            .await
            .expect_err("cancelled");
        assert_eq!(err.code, ErrorCode::Cancelled, "{err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert_eq!(ra.open_txns(), vec![txn], "transaction kept");
    }
}
