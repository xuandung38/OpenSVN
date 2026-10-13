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
/// How many corrupt journals of one working copy are kept aside before quarantine refuses.
const QUARANTINE_SLOTS: usize = 8;

/// Why a journal could not be loaded. Only [`JournalLoadError::Corrupt`] may be set aside; a
/// journal is never deleted because it could not be read.
#[derive(Debug)]
pub enum JournalLoadError {
    /// The file exists but could not be read (permissions, I/O, another program holding it).
    /// It stays; the next open tries again.
    Unreadable(SvnError),
    /// A file that is not a journal of any version: safe to move aside.
    Corrupt(SvnError),
    /// A journal format this build does not know (written by another app version). It stays
    /// untouched and nothing is finalized from it.
    UnsupportedVersion {
        /// Format version found in the file.
        version: u64,
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
}

impl std::fmt::Display for JournalLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error().fmt(f)
    }
}

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

    /// File name derived from the canonical root path: stable whichever spelling of the root
    /// (relative, through a symlink, with `..`) the caller used, and safe whatever characters
    /// the path holds.
    fn file_for(&self, wc_root: &Path) -> PathBuf {
        let digest = Sha1::digest(canonical(wc_root).to_string_lossy().as_bytes());
        let name: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        self.dir.join(format!("{name}.json"))
    }

    /// Starts the journal of a commit of the working copy at `wc_root` and returns the observer
    /// to hand to the commit editor. Fails when a journal of an earlier commit is still there:
    /// that one has to be recovered first.
    pub fn begin(&self, wc_root: &Path, repo_root_url: &str) -> Result<CommitJournal> {
        let path = self.file_for(wc_root);
        if path.exists() {
            if let Err(JournalLoadError::UnsupportedVersion { error, .. }) = self.load(wc_root) {
                return Err(error);
            }
            return Err(SvnError::new(
                ErrorCode::WcNeedsCleanup,
                "an earlier commit of this working copy was interrupted; reopen the working copy to recover it",
            ));
        }
        let record = JournalRecord {
            version: VERSION,
            wc_root: canonical(wc_root).to_string_lossy().into_owned(),
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

    /// Journal of an interrupted commit, if any. The error says whether the file may be set
    /// aside ([`JournalLoadError::Corrupt`]) or must stay where it is.
    pub fn load(
        &self,
        wc_root: &Path,
    ) -> std::result::Result<Option<JournalRecord>, JournalLoadError> {
        let path = self.file_for(wc_root);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(JournalLoadError::Unreadable(SvnError::from_io(
                    format!("read {}", path.display()),
                    &e,
                )));
            }
        };
        let corrupt = |why: String| {
            JournalLoadError::Corrupt(SvnError::new(
                ErrorCode::Corrupt,
                format!("commit journal {} is unreadable: {why}", path.display()),
            ))
        };
        // Every version of the app writes a JSON object with a numeric `version`, atomically. A
        // file without one is damage; a different version is a format this build does not know
        // (written by a newer app) and must not be touched.
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|e| corrupt(e.to_string()))?;
        let Some(version) = value.get("version").and_then(serde_json::Value::as_u64) else {
            return Err(corrupt("no format version".to_owned()));
        };
        if version != u64::from(VERSION) {
            return Err(JournalLoadError::UnsupportedVersion {
                version,
                error: SvnError::new(
                    ErrorCode::WcNeedsCleanup,
                    format!(
                        "an interrupted commit of this working copy was recorded by another version \
                         of OpenSVN (journal format {version}, this version reads {VERSION}); open \
                         the working copy with that version to finish it, or move {} away to \
                         discard it",
                        path.display()
                    ),
                ),
            });
        }
        let record: JournalRecord =
            serde_json::from_value(value).map_err(|e| corrupt(e.to_string()))?;
        Ok(Some(record))
    }

    /// Moves a corrupt journal of `wc_root` aside so it no longer blocks commits but stays
    /// available for diagnosis. The first free name of `<name>.json.corrupt`,
    /// `<name>.json.corrupt.1`, ... is used: an earlier quarantined journal is never replaced.
    /// On failure the journal stays where it is.
    pub fn quarantine(&self, wc_root: &Path) -> Result<PathBuf> {
        let path = self.file_for(wc_root);
        let aside = (0..QUARANTINE_SLOTS)
            .map(|n| match n {
                0 => path.with_extension("json.corrupt"),
                n => path.with_extension(format!("json.corrupt.{n}")),
            })
            .find(|p| is_free(p))
            .ok_or_else(|| {
                SvnError::new(
                    ErrorCode::WcNeedsCleanup,
                    format!(
                        "no free name to set the commit journal {} aside; remove old *.json.corrupt* files",
                        path.display()
                    ),
                )
            })?;
        std::fs::rename(&path, &aside).map_err(|e| {
            SvnError::from_io(
                format!("move {} to {}", path.display(), aside.display()),
                &e,
            )
        })?;
        Ok(aside)
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

/// Whether nothing (not even a dangling link) exists at `path`.
fn is_free(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

/// Canonical form of a working copy root; the path as given when it cannot be resolved (the
/// key must still be computable for a root that disappeared).
fn canonical(wc_root: &Path) -> PathBuf {
    std::fs::canonicalize(wc_root).unwrap_or_else(|_| wc_root.to_path_buf())
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
