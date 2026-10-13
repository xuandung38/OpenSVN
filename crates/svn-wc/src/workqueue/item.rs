//! Work item codec: exactly the skels svn's `svn_wc__wq_build_*` functions produce.
//!
//! All paths are working-copy-root-relative relpaths with `/` separators. Integers are decimal
//! atoms. Argument order and arity were verified at tags 1.14.5 and 1.15.0 (builders are
//! byte-identical in both).

use crate::skel::Skel;

use super::WorkQueueError;

const OP_FILE_COMMIT: &str = "file-commit";
const OP_FILE_INSTALL: &str = "file-install";
const OP_FILE_REMOVE: &str = "file-remove";
const OP_FILE_MOVE: &str = "file-move";
const OP_FILE_TRANSLATE: &str = "file-translate";
const OP_SYNC_FILE_FLAGS: &str = "sync-file-flags";
const OP_PREJ_INSTALL: &str = "prej-install";
const OP_DIR_REMOVE: &str = "dir-remove";
const OP_DIR_INSTALL: &str = "dir-install";
const OP_POSTUPGRADE: &str = "postupgrade";
const OP_BASE_REMOVE: &str = "base-remove";
const OP_RECORD_FILEINFO: &str = "record-fileinfo";

/// One work queue item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkItem {
    /// `(file-commit relpath)`: post-commit fixups of the working file.
    FileCommit {
        /// File.
        relpath: String,
    },
    /// `(file-install relpath use_commit_times record_fileinfo [source_relpath])`: write the
    /// working file from the pristine (or from `source`), translating per its properties.
    FileInstall {
        /// File to create.
        relpath: String,
        /// Set the file's mtime to the last-changed date.
        use_commit_times: bool,
        /// Record translated size and mtime in NODES afterwards.
        record_fileinfo: bool,
        /// Install from this file instead of the pristine.
        source: Option<String>,
    },
    /// `(file-remove relpath)`; a missing file is fine.
    FileRemove {
        /// File.
        relpath: String,
    },
    /// `(file-move src dst)`; a missing source means the item already ran.
    FileMove {
        /// Source.
        src: String,
        /// Destination.
        dst: String,
    },
    /// `(file-translate local src dst)`: copy `src` to `dst` translating with the properties of
    /// the versioned file `local` (always repairing line endings).
    FileTranslate {
        /// Versioned file whose properties select the translation.
        local: String,
        /// Source file.
        src: String,
        /// Destination file.
        dst: String,
    },
    /// `(sync-file-flags relpath)`: executable / read-only bits from properties and lock.
    SyncFileFlags {
        /// File.
        relpath: String,
    },
    /// `(prej-install relpath [conflict skel])`: write the `.prej` property reject file. The
    /// trailing skel only exists in queues written by svn 1.7/1.8.
    PrejInstall {
        /// Node with the property conflict.
        relpath: String,
        /// Legacy property conflict skel.
        legacy_conflict: Option<Skel>,
    },
    /// `(dir-install relpath)`: create the directory.
    DirInstall {
        /// Directory.
        relpath: String,
    },
    /// `(dir-remove relpath [1])`: trailing `1` means recursive.
    DirRemove {
        /// Directory.
        relpath: String,
        /// Remove the whole tree.
        recursive: bool,
    },
    /// `(postupgrade)`: rewrites the legacy `format`/`entries` files after an upgrade.
    PostUpgrade,
    /// Legacy `(base-remove relpath value [second])`. With two integers the first is the
    /// not-present revision (`-1` = none); with one it is the old `keep_not_present` flag.
    BaseRemove {
        /// Node.
        relpath: String,
        /// First integer.
        value: i64,
        /// Second integer, present in the modern form.
        second: Option<i64>,
    },
    /// Legacy `(record-fileinfo relpath [mtime])`: optionally set the mtime, then record the
    /// file's size and mtime.
    RecordFileinfo {
        /// File.
        relpath: String,
        /// APR microseconds mtime to set before recording; `0`/absent leaves it alone.
        mtime: Option<i64>,
    },
}

fn atom_str(op: &str, s: &Skel) -> Result<String, WorkQueueError> {
    let bytes = s
        .as_atom()
        .ok_or_else(|| WorkQueueError::malformed(op, "expected an atom"))?;
    String::from_utf8(bytes.to_vec())
        .map_err(|_| WorkQueueError::malformed(op, "path is not UTF-8"))
}

fn atom_int(op: &str, s: &Skel) -> Result<i64, WorkQueueError> {
    atom_str(op, s)?
        .parse()
        .map_err(|_| WorkQueueError::malformed(op, "expected a decimal integer"))
}

fn arity(op: &str, args: &[Skel], min: usize, max: usize) -> Result<(), WorkQueueError> {
    if args.len() < min || args.len() > max {
        return Err(WorkQueueError::malformed(
            op,
            format!("expected {min}..={max} arguments, found {}", args.len()),
        ));
    }
    Ok(())
}

fn s(text: &str) -> Skel {
    Skel::atom(text.as_bytes())
}

fn int(v: i64) -> Skel {
    Skel::atom(v.to_string().into_bytes())
}

fn flag(b: bool) -> Skel {
    int(i64::from(b))
}

impl WorkItem {
    /// Operation name as stored in the queue.
    pub fn op_name(&self) -> &'static str {
        match self {
            Self::FileCommit { .. } => OP_FILE_COMMIT,
            Self::FileInstall { .. } => OP_FILE_INSTALL,
            Self::FileRemove { .. } => OP_FILE_REMOVE,
            Self::FileMove { .. } => OP_FILE_MOVE,
            Self::FileTranslate { .. } => OP_FILE_TRANSLATE,
            Self::SyncFileFlags { .. } => OP_SYNC_FILE_FLAGS,
            Self::PrejInstall { .. } => OP_PREJ_INSTALL,
            Self::DirInstall { .. } => OP_DIR_INSTALL,
            Self::DirRemove { .. } => OP_DIR_REMOVE,
            Self::PostUpgrade => OP_POSTUPGRADE,
            Self::BaseRemove { .. } => OP_BASE_REMOVE,
            Self::RecordFileinfo { .. } => OP_RECORD_FILEINFO,
        }
    }

    /// Builds the skel svn would write for this item.
    pub fn to_skel(&self) -> Skel {
        let mut v = vec![s(self.op_name())];
        match self {
            Self::FileCommit { relpath }
            | Self::FileRemove { relpath }
            | Self::SyncFileFlags { relpath }
            | Self::DirInstall { relpath } => v.push(s(relpath)),
            Self::FileInstall {
                relpath,
                use_commit_times,
                record_fileinfo,
                source,
            } => {
                v.extend([s(relpath), flag(*use_commit_times), flag(*record_fileinfo)]);
                v.extend(source.iter().map(|p| s(p)));
            }
            Self::FileMove { src, dst } => v.extend([s(src), s(dst)]),
            Self::FileTranslate { local, src, dst } => v.extend([s(local), s(src), s(dst)]),
            Self::PrejInstall {
                relpath,
                legacy_conflict,
            } => {
                v.push(s(relpath));
                v.extend(legacy_conflict.iter().cloned());
            }
            Self::DirRemove { relpath, recursive } => {
                v.push(s(relpath));
                if *recursive {
                    v.push(int(1));
                }
            }
            Self::PostUpgrade => {}
            Self::BaseRemove {
                relpath,
                value,
                second,
            } => {
                v.extend([s(relpath), int(*value)]);
                v.extend(second.iter().map(|x| int(*x)));
            }
            Self::RecordFileinfo { relpath, mtime } => {
                v.push(s(relpath));
                v.extend(mtime.iter().map(|x| int(*x)));
            }
        }
        Skel::list(v)
    }

    /// Serialized form stored in `WORK_QUEUE.work`.
    pub fn encode(&self) -> Vec<u8> {
        self.to_skel().unparse()
    }

    /// Parses the stored bytes of one work item.
    pub fn decode(bytes: &[u8]) -> Result<Self, WorkQueueError> {
        let skel = Skel::parse(bytes)
            .map_err(|e| WorkQueueError::malformed("?", format!("not a skel: {}", e.message)))?;
        Self::from_skel(&skel)
    }

    /// Interprets a parsed skel. Unknown operation names (including the never-persisted
    /// `tmp-set-*` markers) yield [`WorkQueueError::UnknownItem`].
    pub fn from_skel(skel: &Skel) -> Result<Self, WorkQueueError> {
        let list = skel
            .as_list()
            .ok_or_else(|| WorkQueueError::malformed("?", "a work item is a list"))?;
        let (head, args) = list
            .split_first()
            .ok_or_else(|| WorkQueueError::malformed("?", "empty work item"))?;
        let op = atom_str("?", head)?;
        let path = |i: usize| atom_str(&op, &args[i]);
        Ok(match op.as_str() {
            OP_FILE_COMMIT => {
                // Newer-than-1.7 builders emit only the relpath; legacy items carry extra
                // values that svn ignores.
                arity(&op, args, 1, 3)?;
                Self::FileCommit { relpath: path(0)? }
            }
            OP_FILE_INSTALL => {
                arity(&op, args, 3, 4)?;
                Self::FileInstall {
                    relpath: path(0)?,
                    use_commit_times: atom_int(&op, &args[1])? != 0,
                    record_fileinfo: atom_int(&op, &args[2])? != 0,
                    source: if args.len() == 4 {
                        Some(path(3)?)
                    } else {
                        None
                    },
                }
            }
            OP_FILE_REMOVE => {
                arity(&op, args, 1, 1)?;
                Self::FileRemove { relpath: path(0)? }
            }
            OP_FILE_MOVE => {
                arity(&op, args, 2, 2)?;
                Self::FileMove {
                    src: path(0)?,
                    dst: path(1)?,
                }
            }
            OP_FILE_TRANSLATE => {
                arity(&op, args, 3, 3)?;
                Self::FileTranslate {
                    local: path(0)?,
                    src: path(1)?,
                    dst: path(2)?,
                }
            }
            OP_SYNC_FILE_FLAGS => {
                arity(&op, args, 1, 1)?;
                Self::SyncFileFlags { relpath: path(0)? }
            }
            OP_PREJ_INSTALL => {
                arity(&op, args, 1, 2)?;
                Self::PrejInstall {
                    relpath: path(0)?,
                    legacy_conflict: args.get(1).cloned(),
                }
            }
            OP_DIR_INSTALL => {
                arity(&op, args, 1, 1)?;
                Self::DirInstall { relpath: path(0)? }
            }
            OP_DIR_REMOVE => {
                arity(&op, args, 1, 2)?;
                Self::DirRemove {
                    relpath: path(0)?,
                    recursive: match args.get(1) {
                        Some(v) => atom_int(&op, v)? != 0,
                        None => false,
                    },
                }
            }
            OP_POSTUPGRADE => {
                arity(&op, args, 0, 0)?;
                Self::PostUpgrade
            }
            OP_BASE_REMOVE => {
                arity(&op, args, 2, 3)?;
                Self::BaseRemove {
                    relpath: path(0)?,
                    value: atom_int(&op, &args[1])?,
                    second: match args.get(2) {
                        Some(v) => Some(atom_int(&op, v)?),
                        None => None,
                    },
                }
            }
            OP_RECORD_FILEINFO => {
                arity(&op, args, 1, 2)?;
                Self::RecordFileinfo {
                    relpath: path(0)?,
                    mtime: match args.get(1) {
                        Some(v) => Some(atom_int(&op, v)?),
                        None => None,
                    },
                }
            }
            _ => return Err(WorkQueueError::UnknownItem(op)),
        })
    }
}
