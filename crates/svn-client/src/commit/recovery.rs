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
//!   dropped. Transaction gone: the log after the journal's start revision is searched for the
//!   commit (author when known, log message and changed paths); exactly one match is finalized,
//!   anything else keeps the journal and reports it as needing the user;
//! * only a transaction is recorded: it is deleted on the server and the journal is dropped;
//! * nothing reached the server: the journal is dropped.
//!
//! A journal whose outcome is unknown is never dropped without that check, and no journal is
//! dropped because it could not be read: a file of an unknown format version stays untouched,
//! a corrupt one is moved aside under a new name, an unreadable one stays.

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
            Ok(Outcome::Finalized) => report.finalized.push(id),
            Ok(Outcome::Aborted) => report.aborted.push(id),
            Ok(Outcome::NeedsUser(e)) => report.needs_user.push((id, e)),
            Err(e) => report.failed.push((id, e)),
        }
    }
    Ok(report)
}

/// Processes the journals of the working copy `db` in `state_dir` (other working copies'
/// journals are not touched; a corrupt journal, whose working copy cannot be known, is moved
/// aside and reported).
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
        let data = match JournalStore::load(&path) {
            Ok(d) if d.wcroot == db.wcroot() => d,
            Ok(_) => continue,
            Err(JournalLoadError::UnsupportedVersion { wcroot, error, .. }) => {
                if wcroot.as_deref() == Some(db.wcroot()) {
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
            Ok(Outcome::Finalized) => report.finalized.push(id),
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
/// format this build does not know, and while a journal file cannot be read at all (its working
/// copy is unknown, so it may be this one with a `MERGE` already sent): committing again before
/// it is resolved could commit the same change twice or build on a BASE that misses a revision.
/// Journals of other working copies, journals that only name a transaction and corrupt files
/// (recovery sets them aside and reports them) are left to recovery.
pub(crate) fn finalize_pending_for_wc(store: &JournalStore, db: &WcDb) -> Result<()> {
    for path in store.list()? {
        let data = match JournalStore::load(&path) {
            Ok(d) => d,
            Err(JournalLoadError::UnsupportedVersion { wcroot, error, .. })
                if wcroot.as_deref().is_none_or(|w| w == db.wcroot()) =>
            {
                return Err(error);
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
            Err(_) => continue,
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
    Finalized,
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
    if data.merged.is_some() {
        finalize_journal(store, &data, wc, cancel)?;
        return Ok(Outcome::Finalized);
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
                data.merged = Some(merge);
                store.write(&data)?;
                finalize_journal(store, &data, wc, cancel)?;
                Ok(Outcome::Finalized)
            }
            Reconciled::NoMatch(why)
            | Reconciled::Ambiguous(why)
            | Reconciled::NotSearched(why) => Ok(Outcome::NeedsUser(SvnError::new(
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
        ra.abort_txn(txn).await?;
    }
    store.remove(&data.id)?;
    Ok(Outcome::Aborted)
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
    if ra.abort_txn_checked(txn, cancel).await? == TxnAbort::Deleted {
        return Ok(Reconciled::NotCommitted);
    }
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
            "the transaction is gone and {} revisions (r{}..r{head}) were made since the commit \
             started, more than the {max_revisions} the automatic search covers",
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
            "the transaction is gone but none of r{}..r{head} matches the commit",
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
}
