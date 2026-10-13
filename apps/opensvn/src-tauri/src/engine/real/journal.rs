//! Commit journal: a small file per working copy in the app state directory that records how far
//! a commit got, so a crash can be cleaned up when the working copy is opened again.
//!
//! * `Started`: the commit began, no server transaction exists yet. Nothing to undo.
//! * `TxnCreated`: the server transaction exists and is not merged. Recovery deletes it.
//! * `Merged`: the server created the revision but the working copy was not updated yet.
//!   Recovery finalizes the working copy; the transaction must not be deleted.
//!
//! The file is replaced atomically on every step and removed once the working copy is finalized.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use svn_ra_api::{CommitInfo, CommitObserver};
use svn_types::{ErrorCode, Result, SvnError, write_file_replacing};

/// Current journal file format.
const VERSION: u32 = 1;
/// Directory below the state directory that holds the journals.
const DIR: &str = "commit-journal";

/// How far a commit got.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "kebab-case")]
pub enum JournalPhase {
    /// The commit began; no server transaction yet.
    Started,
    /// The server transaction exists and is not merged.
    TxnCreated {
        /// Transaction name.
        txn: String,
    },
    /// The commit was merged into `rev`; the working copy still needs finalizing.
    Merged {
        /// New revision.
        rev: u64,
    },
}

/// Contents of a journal file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalRecord {
    /// File format version.
    pub version: u32,
    /// Canonical working copy root the commit belongs to.
    pub wc_root: String,
    /// Repository root URL (where an abandoned transaction is deleted).
    pub repo_root_url: String,
    /// Progress of the commit.
    #[serde(flatten)]
    pub phase: JournalPhase,
}

/// Journal files below the app state directory.
#[derive(Debug, Clone)]
pub struct JournalStore {
    dir: PathBuf,
}

impl JournalStore {
    /// Store keeping its files below `<state_dir>/commit-journal`.
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            dir: state_dir.join(DIR),
        }
    }

    /// File name derived from the root path: stable, and safe whatever characters the path holds.
    fn file_for(&self, wc_root: &Path) -> PathBuf {
        let digest = Sha1::digest(wc_root.to_string_lossy().as_bytes());
        let name: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        self.dir.join(format!("{name}.json"))
    }

    /// Starts the journal of a commit of the working copy at `wc_root` and returns the observer
    /// to hand to the commit editor. Fails when a journal of an earlier commit is still there:
    /// that one has to be recovered first.
    pub fn begin(&self, wc_root: &Path, repo_root_url: &str) -> Result<CommitJournal> {
        let path = self.file_for(wc_root);
        if path.exists() {
            return Err(SvnError::new(
                ErrorCode::WcNeedsCleanup,
                "an earlier commit of this working copy was interrupted; reopen the working copy to recover it",
            ));
        }
        let record = JournalRecord {
            version: VERSION,
            wc_root: wc_root.to_string_lossy().into_owned(),
            repo_root_url: repo_root_url.to_owned(),
            phase: JournalPhase::Started,
        };
        std::fs::create_dir_all(&self.dir).map_err(|e| {
            SvnError::from_io(
                format!("create journal directory {}", self.dir.display()),
                &e,
            )
        })?;
        write_record(&path, &record)?;
        Ok(CommitJournal {
            path,
            record: Mutex::new(record),
        })
    }

    /// Journal of an interrupted commit, if any.
    pub fn load(&self, wc_root: &Path) -> Result<Option<JournalRecord>> {
        let path = self.file_for(wc_root);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(SvnError::from_io(format!("read {}", path.display()), &e)),
        };
        let record: JournalRecord = serde_json::from_slice(&bytes).map_err(|e| {
            SvnError::new(
                ErrorCode::Corrupt,
                format!("commit journal {} is unreadable: {e}", path.display()),
            )
        })?;
        if record.version != VERSION {
            return Err(SvnError::new(
                ErrorCode::Corrupt,
                format!(
                    "commit journal {} has version {}",
                    path.display(),
                    record.version
                ),
            ));
        }
        Ok(Some(record))
    }

    /// Removes the journal of `wc_root` (no error when there is none).
    pub fn clear(&self, wc_root: &Path) -> Result<()> {
        let path = self.file_for(wc_root);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SvnError::from_io(format!("remove {}", path.display()), &e)),
        }
    }
}

fn write_record(path: &Path, record: &JournalRecord) -> Result<()> {
    let data = serde_json::to_vec(record).map_err(|e| {
        SvnError::new(
            ErrorCode::Internal,
            format!("cannot encode commit journal: {e}"),
        )
    })?;
    write_file_replacing(path, &data)
}

/// Journal of the commit in progress; give it to the commit editor as its observer. Each step is
/// on disk before the editor continues, so a crash leaves the last completed step behind.
#[derive(Debug)]
pub struct CommitJournal {
    path: PathBuf,
    record: Mutex<JournalRecord>,
}

impl CommitJournal {
    fn advance(&self, phase: JournalPhase) -> Result<()> {
        let mut record = self.record.lock().unwrap_or_else(PoisonError::into_inner);
        let next = JournalRecord {
            phase,
            ..record.clone()
        };
        write_record(&self.path, &next)?;
        *record = next;
        Ok(())
    }

    /// The commit finished and the working copy is up to date: removes the journal.
    pub fn complete(self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SvnError::from_io(
                format!("remove {}", self.path.display()),
                &e,
            )),
        }
    }
}

impl CommitObserver for CommitJournal {
    fn on_txn_created(&self, txn: &str) -> Result<()> {
        self.advance(JournalPhase::TxnCreated {
            txn: txn.to_owned(),
        })
    }

    fn on_merged(&self, info: &CommitInfo) -> Result<()> {
        self.advance(JournalPhase::Merged {
            rev: info.revision.0,
        })
    }
}
