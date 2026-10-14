//! Commit journal: a durable record that lets a crash after the server merge be finished.
//!
//! One JSON file per commit lives in the application state directory, next to a data directory
//! holding the repository-normal texts that were uploaded. The file is rewritten atomically
//! (temp file, fsync, rename, directory fsync) at four points:
//!
//! 1. before the server transaction exists: the committables, text digests, log message and the
//!    youngest revision before the commit;
//! 2. when the transaction is created: its name, so a leaked transaction can be deleted;
//! 3. right before the `MERGE` is sent (`merge_sent`): from here on the commit may have become a
//!    revision even when no answer arrives;
//! 4. right after MERGE: the new revision, so the working copy can be finalized.
//!
//! Recovery (`recovery.rs`) reads the journal: a merged one is finalized against the working
//! copy; one with `merge_sent` but no revision is reconciled with the server (transaction still
//! there: deleted and dropped; gone: the log is searched for the commit); one that only names a
//! transaction is aborted on the server; anything else is dropped.
//!
//! Safety rules (shared with the app's former journal): a journal is never deleted because it
//! could not be read. A file of a format version this build does not know is left untouched
//! (another build wrote it); a file of a known version that does not parse is corrupt and is
//! moved aside under a new name, never replacing an earlier one; a file that cannot be read at
//! all (permissions, I/O) stays where it is.
//!
//! File names carry an owner hint: `commit-<hint>-<unique>.json`, where `<hint>` is the first 12
//! hex digits of the SHA-1 of the working copy root ([`owner_hint`]). A file that cannot be read
//! or parsed is attributed to its working copy through the name alone, so an unreadable journal
//! of one working copy does not block or get reported for every other one. Files without a hint
//! (written by earlier builds, or renamed by hand) may belong to any working copy and are treated
//! conservatively as such ([`JournalStore::may_belong_to`]).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use svn_ra_api::{CommitInfo, CommitObserver};
use svn_types::{ErrorCode, Result, SvnError};

use super::collect::{ItemAction, ItemKind};

/// Format version written by this build.
///
/// Any change to what is written ([`JournalData`] and the types in it, their serde attributes or
/// the meaning of a field) MUST bump this, and older versions this build still understands stay
/// in [`READABLE_VERSIONS`]. A build that meets a version it does not know leaves the file alone,
/// so a journal from a newer build is never misread (version 2 added `merge_sent`: a version 1
/// reader would drop such a journal as "never merged").
const FORMAT_VERSION: u32 = 2;
/// Versions this build reads (1: before `merge_sent`, `start_youngest`, `message`, `author`).
const READABLE_VERSIONS: [u32; 2] = [1, 2];
const JOURNAL_PREFIX: &str = "commit-";
const JOURNAL_SUFFIX: &str = ".json";
/// Hex digits of the owner hint in journal file names.
const OWNER_HINT_LEN: usize = 12;
/// How many quarantined copies of one journal may exist before quarantine refuses.
const QUARANTINE_SLOTS: usize = 8;

/// Text that was uploaded for a file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JournalText {
    /// SHA-1 of the repository-normal text, lower-case hex.
    pub sha1: String,
    /// MD5, lower-case hex.
    pub md5: String,
    /// Size in bytes.
    pub size: u64,
    /// File name of the saved text inside the commit's data directory.
    pub data: String,
}

/// One committed path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JournalItem {
    /// Working copy relative path.
    pub relpath: String,
    /// Repository relative path.
    pub repos_relpath: String,
    /// Kind of node.
    pub kind: ItemKind,
    /// Operation.
    pub action: ItemAction,
    /// Uploaded text, if any.
    pub text: Option<JournalText>,
}

/// A lock token sent with the commit; its working copy lock row is dropped by the finalize
/// transaction unless the locks are kept.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JournalLock {
    /// Working copy relative path of the locked node.
    pub relpath: String,
    /// Lock token.
    pub token: String,
}

/// Outcome of the server merge.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JournalMerge {
    /// New revision.
    pub revision: u64,
    /// Commit date, APR microseconds.
    pub date: Option<i64>,
    /// Committing user.
    pub author: Option<String>,
}

/// Contents of a journal file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JournalData {
    /// Format version of the file.
    pub version: u32,
    /// Identifier, also part of the file and directory names.
    pub id: String,
    /// Absolute working copy root.
    pub wcroot: PathBuf,
    /// Keep locks after the commit.
    pub keep_locks: bool,
    /// Keep changelist membership after the commit.
    pub keep_changelist: bool,
    /// Server transaction name, once created.
    pub txn: Option<String>,
    /// Set after MERGE.
    pub merged: Option<JournalMerge>,
    /// Committed paths, sorted parents first.
    pub items: Vec<JournalItem>,
    /// Every lock token sent with the commit (committed paths, unmodified locked files met by
    /// the harvest, locked nodes below deleted or replaced directories).
    #[serde(default)]
    pub locks: Vec<JournalLock>,
    /// The `MERGE` may have been sent: until `merged` is set the outcome is unknown and the
    /// journal must only be dropped once the server shows the commit did not happen.
    #[serde(default)]
    pub merge_sent: bool,
    /// Youngest revision of the repository before the transaction was created: a revision made
    /// by this commit is newer.
    #[serde(default)]
    pub start_youngest: Option<u64>,
    /// Log message of the commit, to recognize it in the log.
    #[serde(default)]
    pub message: Option<String>,
    /// User name the server records as author, when the caller knows it.
    #[serde(default)]
    pub author: Option<String>,
}

impl JournalData {
    /// The outcome of the `MERGE` is unknown: it may have been sent and no revision is recorded.
    ///
    /// A version 1 journal has no `merge_sent` marker: one that names a transaction but no
    /// revision may have been left by a crash during the `MERGE`, so it counts as unknown too
    /// and is settled by the server (transaction deleted now, or the log), never aborted blindly.
    pub fn outcome_unknown(&self) -> bool {
        self.merged.is_none() && (self.merge_sent || (self.version == 1 && self.txn.is_some()))
    }
}

/// Why a journal file could not be loaded. Only [`JournalLoadError::Corrupt`] may be moved aside
/// ([`JournalStore::quarantine`]); no variant allows deleting the file.
#[derive(Debug)]
pub enum JournalLoadError {
    /// The file exists but could not be read (permissions, I/O, another program holding it).
    /// It stays; the next attempt tries again.
    Unreadable(SvnError),
    /// The file is not a journal of any version, or a journal of a known version that does not
    /// parse: safe to move aside.
    Corrupt(SvnError),
    /// A format version this build does not know (written by another build). It stays untouched
    /// and nothing is done from it.
    UnsupportedVersion {
        /// Format version found in the file.
        version: u64,
        /// Working copy root named by the file, when it has a readable `wcroot` field.
        wcroot: Option<PathBuf>,
        /// Error explaining what the user can do.
        error: SvnError,
    },
}

impl JournalLoadError {
    /// The underlying error.
    pub fn error(&self) -> &SvnError {
        match self {
            Self::Unreadable(e) | Self::Corrupt(e) | Self::UnsupportedVersion { error: e, .. } => e,
        }
    }

    /// The underlying error, owned.
    pub fn into_error(self) -> SvnError {
        match self {
            Self::Unreadable(e) | Self::Corrupt(e) | Self::UnsupportedVersion { error: e, .. } => e,
        }
    }
}

impl std::fmt::Display for JournalLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error().fmt(f)
    }
}

/// Directory that stores journals and their data.
#[derive(Debug, Clone)]
pub struct JournalStore {
    dir: PathBuf,
}

fn io(context: impl std::fmt::Display, e: &std::io::Error) -> SvnError {
    SvnError::from_io(context, e)
}

fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{nanos:x}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Owner hint of the working copy at `wcroot` for journal file names: the first 12 hex digits of
/// the SHA-1 of the root path exactly as journals record it ([`JournalData::wcroot`]).
pub fn owner_hint(wcroot: &Path) -> String {
    use sha1::{Digest, Sha1};
    let digest = Sha1::digest(wcroot.to_string_lossy().as_bytes());
    let mut hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex.truncate(OWNER_HINT_LEN);
    hex
}

/// The owner hint in the name of the journal file `path`, when it has one: the name continues
/// after `commit-` with exactly 12 lower-case hex digits and a `-`. Identifiers of earlier builds
/// start with the 16 hex digits of a nanosecond timestamp, so they never look like a hint.
fn file_owner_hint(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix(JOURNAL_PREFIX)?;
    let hint = rest.get(..OWNER_HINT_LEN)?;
    let is_hint = hint
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && rest.as_bytes().get(OWNER_HINT_LEN) == Some(&b'-');
    is_hint.then_some(hint)
}

impl JournalStore {
    /// Uses `dir` (created on demand).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Fresh identifier for a new commit, without an owner hint (see [`Self::new_id_for`]).
    pub fn new_id(&self) -> String {
        new_id()
    }

    /// Fresh identifier for a new commit of the working copy at `wcroot`, starting with its
    /// [`owner_hint`] so the file can be attributed without being read.
    pub fn new_id_for(&self, wcroot: &Path) -> String {
        format!("{}-{}", owner_hint(wcroot), new_id())
    }

    /// Whether the journal file `path` may belong to the working copy at `wcroot`, judged from
    /// its name only: true when the name carries that working copy's owner hint or no hint at all
    /// (an earlier build's file, whose owner is unknown until it is read).
    pub fn may_belong_to(path: &Path, wcroot: &Path) -> bool {
        file_owner_hint(path).is_none_or(|h| h == owner_hint(wcroot))
    }

    /// Path of the journal file with `id`.
    pub fn file_of(&self, id: &str) -> PathBuf {
        self.dir
            .join(format!("{JOURNAL_PREFIX}{id}{JOURNAL_SUFFIX}"))
    }

    /// Directory with the saved texts of commit `id`.
    pub fn data_dir_of(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{JOURNAL_PREFIX}{id}"))
    }

    /// Creates the data directory of commit `id`.
    pub fn create_data_dir(&self, id: &str) -> Result<PathBuf> {
        let d = self.data_dir_of(id);
        fs::create_dir_all(&d).map_err(|e| io(format!("create '{}'", d.display()), &e))?;
        Ok(d)
    }

    /// Writes `data` atomically and durably.
    pub fn write(&self, data: &JournalData) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .map_err(|e| io(format!("create '{}'", self.dir.display()), &e))?;
        let bytes = serde_json::to_vec_pretty(data).map_err(|e| {
            SvnError::new(ErrorCode::Internal, format!("cannot encode journal: {e}"))
        })?;
        let mut tmp = tempfile::Builder::new()
            .prefix("journal-")
            .tempfile_in(&self.dir)
            .map_err(|e| io(format!("create temp file in '{}'", self.dir.display()), &e))?;
        tmp.write_all(&bytes).map_err(|e| io("write journal", &e))?;
        tmp.as_file()
            .sync_all()
            .map_err(|e| io("sync journal", &e))?;
        let dest = self.file_of(&data.id);
        tmp.persist(&dest)
            .map_err(|e| io("persist journal", &e.error))?;
        sync_dir(&self.dir);
        Ok(())
    }

    /// Reads one journal file; any [`JournalLoadError`] becomes its error.
    pub fn read(path: &Path) -> Result<JournalData> {
        Self::load(path).map_err(JournalLoadError::into_error)
    }

    /// Reads one journal file and says, on failure, what may be done with it.
    pub fn load(path: &Path) -> std::result::Result<JournalData, JournalLoadError> {
        let bytes = fs::read(path).map_err(|e| {
            JournalLoadError::Unreadable(io(format!("read journal '{}'", path.display()), &e))
        })?;
        let corrupt = |why: String| {
            JournalLoadError::Corrupt(SvnError::new(
                ErrorCode::Corrupt,
                format!("commit journal '{}' is unreadable: {why}", path.display()),
            ))
        };
        // Every version is a JSON object with a numeric `version`, written atomically: a file
        // without one is damage, an unknown version is another build's format.
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|e| corrupt(e.to_string()))?;
        let Some(version) = value.get("version").and_then(serde_json::Value::as_u64) else {
            return Err(corrupt("no format version".to_owned()));
        };
        if !READABLE_VERSIONS.iter().any(|v| u64::from(*v) == version) {
            let wcroot = value
                .get("wcroot")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from);
            return Err(JournalLoadError::UnsupportedVersion {
                version,
                wcroot,
                error: SvnError::new(
                    ErrorCode::WcNeedsCleanup,
                    format!(
                        "an interrupted commit was recorded by another version of OpenSVN \
                         (journal format {version}, this version reads up to {FORMAT_VERSION}); \
                         open the working copy with that version to finish it, or move '{}' \
                         away to discard it",
                        path.display()
                    ),
                ),
            });
        }
        serde_json::from_value(value).map_err(|e| corrupt(e.to_string()))
    }

    /// Moves the corrupt journal file `path` aside so it no longer counts as a journal but stays
    /// available for diagnosis. The first free name of `<file>.corrupt`, `<file>.corrupt.1`, ...
    /// is used: an earlier quarantined file is never replaced. Its data directory is kept. On
    /// failure the file stays where it is.
    pub fn quarantine(&self, path: &Path) -> Result<PathBuf> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        for n in 0..QUARANTINE_SLOTS {
            let aside = match n {
                0 => path.with_file_name(format!("{name}.corrupt")),
                n => path.with_file_name(format!("{name}.corrupt.{n}")),
            };
            match move_no_replace(path, &aside) {
                Ok(()) => return Ok(aside),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(io(
                        format!("move '{}' to '{}'", path.display(), aside.display()),
                        &e,
                    ));
                }
            }
        }
        Err(SvnError::new(
            ErrorCode::WcNeedsCleanup,
            format!(
                "no free name to set the commit journal '{}' aside; remove old *.corrupt* files",
                path.display()
            ),
        ))
    }

    /// Journal files present, sorted by name (creation order).
    pub fn list(&self) -> Result<Vec<PathBuf>> {
        let rd = match fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io(format!("read '{}'", self.dir.display()), &e)),
        };
        let mut out: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(JOURNAL_PREFIX) && n.ends_with(JOURNAL_SUFFIX))
            })
            .collect();
        out.sort();
        Ok(out)
    }

    /// Deletes the journal file and the data directory of `id`.
    pub fn remove(&self, id: &str) -> Result<()> {
        let data = self.data_dir_of(id);
        if let Err(e) = fs::remove_dir_all(&data)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(io(format!("remove '{}'", data.display()), &e));
        }
        let file = self.file_of(id);
        match fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(format!("remove '{}'", file.display()), &e)),
        }
        sync_dir(&self.dir);
        Ok(())
    }

    /// Initial journal for a commit that has not reached the server yet.
    pub fn begin(
        &self,
        id: String,
        wcroot: PathBuf,
        keep_locks: bool,
        keep_changelist: bool,
        items: Vec<JournalItem>,
        locks: Vec<JournalLock>,
    ) -> JournalData {
        JournalData {
            version: FORMAT_VERSION,
            id,
            wcroot,
            keep_locks,
            keep_changelist,
            txn: None,
            merged: None,
            items,
            locks,
            merge_sent: false,
            start_youngest: None,
            message: None,
            author: None,
        }
    }
}

/// Moves `from` to `to` without ever replacing an existing `to`: fails with
/// [`std::io::ErrorKind::AlreadyExists`] when something is there, even if it appeared after the
/// caller looked. `std::fs::rename` cannot be used for this: it silently replaces the target on
/// Unix and (with `MOVEFILE_REPLACE_EXISTING`) on Windows.
///
/// A hard link is created first (`link(2)` / `CreateHardLinkW` both refuse an existing target
/// atomically), then the original is removed. On file systems without hard links the contents
/// are copied into a file opened with `create_new` (`O_EXCL` / `CREATE_NEW`), which is just as
/// exclusive. If the original cannot be removed afterwards the new name is removed again, so on
/// any error `from` is left as it was. On success the folders of both names are synced so the
/// move survives a crash (where the platform allows it).
pub fn move_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    match fs::hard_link(from, to) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(e),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(e),
        Err(_) => {
            let data = fs::read(from)?;
            let mut out = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(to)?;
            if let Err(e) = out.write_all(&data).and_then(|()| out.sync_all()) {
                drop(out);
                let _ = fs::remove_file(to);
                return Err(e);
            }
        }
    }
    if let Err(e) = fs::remove_file(from) {
        let _ = fs::remove_file(to);
        return Err(e);
    }
    for dir in [to.parent(), from.parent()].into_iter().flatten() {
        sync_dir(dir);
    }
    Ok(())
}

/// Makes a rename durable where the platform allows it (no-op elsewhere).
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(f) = fs::File::open(dir) {
        let _ = f.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Writes the transaction name and the revision into the journal as the RA layer reports them.
pub struct JournalObserver {
    store: JournalStore,
    data: Mutex<JournalData>,
}

impl JournalObserver {
    /// Observer over an already written initial journal.
    pub fn new(store: JournalStore, data: JournalData) -> Self {
        Self {
            store,
            data: Mutex::new(data),
        }
    }

    /// Snapshot of the journal state.
    pub fn snapshot(&self) -> JournalData {
        self.data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl CommitObserver for JournalObserver {
    fn on_txn_created(&self, txn: &str) -> Result<()> {
        let mut data = self
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        data.txn = Some(txn.to_owned());
        self.store.write(&data)
    }

    fn on_merge_sending(&self, _txn: &str) -> Result<()> {
        let mut data = self
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        data.merge_sent = true;
        if let Err(e) = self.store.write(&data) {
            // The MERGE is vetoed; memory keeps matching the file.
            data.merge_sent = false;
            return Err(e);
        }
        failpoint!("commit.after_merge_sent_marker");
        Ok(())
    }

    fn on_merged(&self, info: &CommitInfo) -> Result<()> {
        let mut data = self
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Kept in memory even when the write fails, so the caller can still finalize.
        data.merged = Some(JournalMerge {
            revision: info.revision.0,
            date: info.date.map(|d| d.0),
            author: info.author.clone(),
        });
        failpoint!("commit.journal_merged_write");
        self.store.write(&data)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn item() -> JournalItem {
        JournalItem {
            relpath: "a/b.txt".into(),
            repos_relpath: "trunk/a/b.txt".into(),
            kind: ItemKind::File,
            action: ItemAction::Add,
            text: Some(JournalText {
                sha1: "00".repeat(20),
                md5: "11".repeat(16),
                size: 3,
                data: "0.txt".into(),
            }),
        }
    }

    #[test]
    fn journal_roundtrips_and_lists_in_order() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = JournalStore::new(dir.path());
        let mut data = store.begin(
            "1-a".into(),
            PathBuf::from("/wc"),
            true,
            false,
            vec![item()],
            vec![JournalLock {
                relpath: "a/b.txt".into(),
                token: "opaquelocktoken:x".into(),
            }],
        );
        store.write(&data).expect("write");
        data.txn = Some("7-x".into());
        data.merged = Some(JournalMerge {
            revision: 9,
            date: Some(5),
            author: Some("u".into()),
        });
        store.write(&data).expect("rewrite");
        let second = store.begin(
            "2-a".into(),
            PathBuf::from("/wc"),
            false,
            false,
            Vec::new(),
            Vec::new(),
        );
        store.write(&second).expect("write");

        let files = store.list().expect("list");
        assert_eq!(files.len(), 2);
        assert_eq!(JournalStore::read(&files[0]).expect("read"), data);
        store.remove("1-a").expect("remove");
        assert_eq!(store.list().expect("list").len(), 1);
        store.remove("1-a").expect("remove is idempotent");
    }

    #[test]
    fn version_1_journals_are_still_read() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("commit-1.json");
        let v1 = r#"{"version":1,"id":"1","wcroot":"/wc","keep_locks":false,
            "keep_changelist":false,"txn":"7-x","merged":null,"items":[]}"#;
        std::fs::write(&path, v1).expect("write");
        let data = JournalStore::read(&path).expect("v1");
        assert!(!data.merge_sent);
        // No marker in version 1: a named transaction may have been merged.
        assert!(data.outcome_unknown());
        assert_eq!(data.txn.as_deref(), Some("7-x"));
        assert_eq!(data.start_youngest, None);
        let no_txn = JournalData {
            txn: None,
            ..data.clone()
        };
        assert!(!no_txn.outcome_unknown());
        let merged = JournalData {
            merged: Some(JournalMerge {
                revision: 3,
                date: None,
                author: None,
            }),
            ..data
        };
        assert!(!merged.outcome_unknown());
    }

    #[test]
    fn unknown_versions_are_left_alone() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("commit-9.json");
        let bytes = br#"{"version":9,"wcroot":"/wc","whatever":true}"#;
        std::fs::write(&path, bytes).expect("write");
        match JournalStore::load(&path) {
            Err(JournalLoadError::UnsupportedVersion {
                version, wcroot, ..
            }) => {
                assert_eq!(version, 9);
                assert_eq!(wcroot, Some(PathBuf::from("/wc")));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(std::fs::read(&path).expect("still there"), bytes);
    }

    #[test]
    fn corrupt_journals_are_quarantined_without_replacing() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = JournalStore::new(dir.path());
        let path = store.file_of("x");
        for (n, junk) in [
            b"{ nope".as_slice(),
            b"{\"no\":\"version\"}",
            b"{\"version\":2}",
        ]
        .into_iter()
        .enumerate()
        {
            std::fs::write(&path, junk).expect("write");
            assert!(
                matches!(JournalStore::load(&path), Err(JournalLoadError::Corrupt(_))),
                "{n}"
            );
            let aside = store.quarantine(&path).expect("quarantine");
            assert!(!path.exists());
            assert_eq!(std::fs::read(&aside).expect("kept"), junk, "{n}");
        }
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .expect("rd")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 3, "earlier quarantined files kept: {names:?}");
        assert!(
            store.list().expect("list").is_empty(),
            "no longer a journal"
        );
    }

    #[test]
    fn journals_that_cannot_be_read_stay() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("commit-d.json");
        std::fs::create_dir(&path).expect("a directory cannot be read as a file");
        assert!(matches!(
            JournalStore::load(&path),
            Err(JournalLoadError::Unreadable(_))
        ));
        assert!(path.exists());
    }

    #[test]
    fn unreadable_journal_is_reported_as_corrupt() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("commit-x.json");
        std::fs::write(&path, b"{ nope").expect("write");
        let err = JournalStore::read(&path).expect_err("corrupt");
        assert_eq!(err.code, ErrorCode::Corrupt);
    }

    #[test]
    fn file_names_carry_an_owner_hint_of_their_working_copy() {
        let store = JournalStore::new("/state");
        let (a, b) = (Path::new("/wc/a"), Path::new("/wc/b"));
        let id = store.new_id_for(a);
        let file = store.file_of(&id);
        assert!(id.starts_with(&owner_hint(a)), "{id}");
        assert_eq!(owner_hint(a).len(), 12);
        assert_ne!(owner_hint(a), owner_hint(b));
        assert!(JournalStore::may_belong_to(&file, a));
        assert!(!JournalStore::may_belong_to(&file, b));
        // Without a hint (earlier builds, hand-made names) the owner is unknown: any working copy.
        for old in [
            store.file_of(&store.new_id()),
            PathBuf::from("/state/commit-junk.json"),
            PathBuf::from("/state/commit-0123456789ab.json"),
            PathBuf::from("/state/commit-0123456789AB-1.json"),
            PathBuf::from("/state/commit-0123456789abc-1.json"),
        ] {
            assert!(JournalStore::may_belong_to(&old, a), "{}", old.display());
            assert!(JournalStore::may_belong_to(&old, b), "{}", old.display());
        }
    }

    #[test]
    fn move_no_replace_never_replaces_and_keeps_the_source_on_failure() {
        let dir = tempfile::tempdir().expect("tmp");
        let (from, to) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&from, b"a").expect("write");
        std::fs::write(&to, b"b").expect("write");
        let err = move_no_replace(&from, &to).expect_err("target exists");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&from).expect("kept"), b"a");
        assert_eq!(std::fs::read(&to).expect("kept"), b"b");
        std::fs::remove_file(&to).expect("rm");
        move_no_replace(&from, &to).expect("moved");
        assert!(!from.exists());
        assert_eq!(std::fs::read(&to).expect("moved"), b"a");
    }
}
