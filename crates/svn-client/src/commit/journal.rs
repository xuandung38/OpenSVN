//! Commit journal: a durable record that lets a crash after the server merge be finished.
//!
//! One JSON file per commit lives in the application state directory, next to a data directory
//! holding the repository-normal texts that were uploaded. The file is rewritten atomically
//! (temp file, fsync, rename, directory fsync) at three points:
//!
//! 1. before the server transaction exists: the committables and text digests;
//! 2. when the transaction is created: its name, so a leaked transaction can be deleted;
//! 3. right after MERGE: the new revision, so the working copy can be finalized.
//!
//! Recovery (`recovery.rs`) reads the journal: a merged one is finalized against the working
//! copy, one that only names a transaction is aborted on the server, anything else is dropped.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use svn_ra_api::{CommitInfo, CommitObserver};
use svn_types::{ErrorCode, Result, SvnError};

use super::collect::{ItemAction, ItemKind};

const FORMAT_VERSION: u32 = 1;
const JOURNAL_PREFIX: &str = "commit-";
const JOURNAL_SUFFIX: &str = ".json";

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
    /// Size and mtime (APR microseconds) of the working file when the text was read; the stat
    /// cache is only stored after the commit when the file still has them.
    pub stat: Option<(i64, i64)>,
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
    /// Lock token that was sent with the commit.
    pub lock_token: Option<String>,
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

impl JournalStore {
    /// Uses `dir` (created on demand).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Fresh identifier for a new commit.
    pub fn new_id(&self) -> String {
        new_id()
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

    /// Reads one journal file.
    pub fn read(path: &Path) -> Result<JournalData> {
        let bytes =
            fs::read(path).map_err(|e| io(format!("read journal '{}'", path.display()), &e))?;
        let data: JournalData = serde_json::from_slice(&bytes).map_err(|e| {
            SvnError::new(
                ErrorCode::Corrupt,
                format!("journal '{}' is unreadable: {e}", path.display()),
            )
        })?;
        if data.version != FORMAT_VERSION {
            return Err(SvnError::new(
                ErrorCode::Corrupt,
                format!(
                    "journal '{}' has unsupported version {}",
                    path.display(),
                    data.version
                ),
            ));
        }
        Ok(data)
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
        }
    }
}

/// Makes a rename durable where the platform allows it (no-op elsewhere).
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(f) = File::open(dir) {
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
        self.store.write(&data)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
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
                stat: Some((3, 9)),
            }),
            lock_token: Some("opaquelocktoken:x".into()),
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
        );
        store.write(&data).expect("write");
        data.txn = Some("7-x".into());
        data.merged = Some(JournalMerge {
            revision: 9,
            date: Some(5),
            author: Some("u".into()),
        });
        store.write(&data).expect("rewrite");
        let second = store.begin("2-a".into(), PathBuf::from("/wc"), false, false, Vec::new());
        store.write(&second).expect("write");

        let files = store.list().expect("list");
        assert_eq!(files.len(), 2);
        assert_eq!(JournalStore::read(&files[0]).expect("read"), data);
        store.remove("1-a").expect("remove");
        assert_eq!(store.list().expect("list").len(), 1);
        store.remove("1-a").expect("remove is idempotent");
    }

    #[test]
    fn unreadable_journal_is_reported_as_corrupt() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("commit-x.json");
        std::fs::write(&path, b"{ nope").expect("write");
        let err = JournalStore::read(&path).expect_err("corrupt");
        assert_eq!(err.code, ErrorCode::Corrupt);
    }
}
