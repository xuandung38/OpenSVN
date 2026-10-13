//! Work queue runner: executes decoded [`WorkItem`]s against the working copy.
//!
//! The runner owns the filesystem side (install, remove, move, translate, flags). Everything
//! that needs wc.db goes through the [`WorkQueueStore`] and [`WorkingCopyOps`] traits, which the
//! `WcDb` facade implements; this keeps the queue logic testable without a database.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use svn_types::{ErrorCode, RelPath, SvnError, rename_replacing, safe_join};

use crate::pristine::{PristineError, PristineIndex, PristineStore, Sha1Digest};
use crate::skel::Skel;
use crate::translate::flags::{self, SyncInput};
use crate::translate::{
    ContentTranslator, KeywordContext, TranslateInfo, UtcOffsetAt, special, translate_stream,
};

use super::{WorkItem, WorkQueueError};

/// Size and mtime of a freshly installed working file, to be written to NODES
/// (`translated_size`, `last_mod_time`) so status can skip hashing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfoRecord {
    /// File.
    pub relpath: String,
    /// Modification time in microseconds since the Unix epoch (APR time).
    pub last_mod_time_micros: i64,
    /// Size of the working file in bytes.
    pub translated_size: i64,
}

/// Persistent queue (the `WORK_QUEUE` table).
pub trait WorkQueueStore {
    /// Atomically (one transaction) deletes the `completed` item, stores `fileinfo`, and returns
    /// the next item by ascending id, or `None` when the queue is empty.
    fn next_item(
        &mut self,
        completed: Option<u64>,
        fileinfo: &[FileInfoRecord],
    ) -> Result<Option<(u64, Vec<u8>)>, WorkQueueError>;
}

/// What the runner needs to know about a node to install its file
/// (`svn_wc__db_read_node_install_info` plus `read_info`).
#[derive(Debug, Clone, Default)]
pub struct NodeInstallInfo {
    /// SHA-1 of the pristine text, if recorded.
    pub checksum: Option<Sha1Digest>,
    /// Current properties (working props of the node).
    pub props: BTreeMap<String, Vec<u8>>,
    /// Last changed date, microseconds since the epoch.
    pub changed_date_micros: Option<i64>,
    /// Last changed revision (`-1` is substituted when unknown, like svn).
    pub changed_rev: Option<i64>,
    /// Last changed author.
    pub changed_author: Option<String>,
    /// Repository path of the node (resolved for added/copied nodes too).
    pub repos_relpath: String,
    /// Repository root URL, URI-encoded.
    pub repos_root_url: String,
    /// The node is a plain addition.
    pub is_added: bool,
    /// The WC holds a lock token for the node.
    pub has_lock: bool,
}

/// Database-backed operations used by work items.
pub trait WorkingCopyOps {
    /// Install information for the node at `relpath`.
    fn install_info(&self, relpath: &str) -> Result<NodeInstallInfo, WorkQueueError>;
    /// Inputs for `sync-file-flags`; `None` when the node is not a normal/added file.
    fn sync_flags_input(&self, relpath: &str) -> Result<Option<SyncInput>, WorkQueueError>;
    /// Writes the `.prej` property reject file recorded in the node's conflict data.
    fn write_prop_reject_file(
        &mut self,
        relpath: &str,
        legacy: Option<&Skel>,
    ) -> Result<(), WorkQueueError>;
    /// Post-commit file fixups (`process_commit_file_install`).
    fn file_commit(&mut self, relpath: &str) -> Result<(), WorkQueueError>;
    /// Removes a BASE node; `not_present_rev` keeps a not-present placeholder at that revision.
    fn base_remove(
        &mut self,
        relpath: &str,
        not_present_rev: Option<i64>,
    ) -> Result<(), WorkQueueError>;
    /// Rewrites the legacy `format`/`entries` files after an upgrade.
    fn post_upgrade(&mut self) -> Result<(), WorkQueueError>;
}

/// Read access to stored pristine texts.
pub trait PristineSource {
    /// Opens the text with this checksum.
    fn open_text(&self, sha1: &Sha1Digest) -> Result<Box<dyn Read>, PristineError>;
}

impl<I: PristineIndex> PristineSource for PristineStore<I> {
    fn open_text(&self, sha1: &Sha1Digest) -> Result<Box<dyn Read>, PristineError> {
        PristineStore::open_text(self, sha1).map(|f| Box::new(f) as Box<dyn Read>)
    }
}

/// Static configuration of a runner.
pub struct RunnerConfig<'a> {
    /// Working copy root.
    pub wc_root: PathBuf,
    /// Resolves the local UTC offset for `$Date$` (see [`UtcOffsetAt`]).
    pub tz: UtcOffsetAt<'a>,
    /// Polled between items; returning `true` stops the run with [`ErrorCode::Cancelled`].
    pub cancelled: Option<&'a dyn Fn() -> bool>,
}

/// Executes the queue.
pub struct WorkQueueRunner<'a> {
    config: RunnerConfig<'a>,
    ops: &'a mut dyn WorkingCopyOps,
    pristine: &'a dyn PristineSource,
    pending_fileinfo: Vec<FileInfoRecord>,
}

impl<'a> WorkQueueRunner<'a> {
    /// Creates a runner.
    pub fn new(
        config: RunnerConfig<'a>,
        ops: &'a mut dyn WorkingCopyOps,
        pristine: &'a dyn PristineSource,
    ) -> Self {
        Self {
            config,
            ops,
            pristine,
            pending_fileinfo: Vec::new(),
        }
    }

    /// Runs items until the queue is empty and returns how many ran. A failing item stays in the
    /// queue and the run stops with [`WorkQueueError::ItemFailed`].
    pub fn run(&mut self, store: &mut dyn WorkQueueStore) -> Result<usize, WorkQueueError> {
        let mut ran = 0;
        let mut completed = None;
        loop {
            let records = std::mem::take(&mut self.pending_fileinfo);
            let next = store.next_item(completed, &records)?;
            if let Some(cancel) = self.config.cancelled
                && cancel()
            {
                return Err(SvnError::new(ErrorCode::Cancelled, "work queue run cancelled").into());
            }
            let Some((id, bytes)) = next else {
                return Ok(ran);
            };
            let outcome = WorkItem::decode(&bytes).and_then(|item| self.execute(&item));
            if let Err(source) = outcome {
                return Err(WorkQueueError::ItemFailed {
                    id,
                    skel: String::from_utf8_lossy(&bytes).into_owned(),
                    source: Box::new(source),
                });
            }
            completed = Some(id);
            ran += 1;
        }
    }

    /// Resolves a relpath from an item below the working copy root, rejecting traversal and
    /// symlinked ancestors.
    fn abs(&self, relpath: &str) -> Result<PathBuf, WorkQueueError> {
        let rel = RelPath::new(relpath)
            .map_err(|e| WorkQueueError::UnsafePath(format!("{relpath}: {}", e.message)))?;
        safe_join(&self.config.wc_root, &rel)
            .map_err(|e| WorkQueueError::UnsafePath(format!("{relpath}: {}", e.message)))
    }

    fn tmp_dir(&self) -> Result<PathBuf, WorkQueueError> {
        let dir = self.config.wc_root.join(crate::db::ADM_DIR).join("tmp");
        fs::create_dir_all(&dir).map_err(WorkQueueError::io("create directory", &dir))?;
        Ok(dir)
    }

    fn execute(&mut self, item: &WorkItem) -> Result<(), WorkQueueError> {
        match item {
            WorkItem::FileInstall {
                relpath,
                use_commit_times,
                record_fileinfo,
                source,
            } => self.file_install(
                relpath,
                *use_commit_times,
                *record_fileinfo,
                source.as_deref(),
            ),
            WorkItem::FileRemove { relpath } => remove_file_if_present(&self.abs(relpath)?),
            WorkItem::FileMove { src, dst } => move_file(&self.abs(src)?, &self.abs(dst)?),
            WorkItem::FileTranslate { local, src, dst } => self.file_translate(local, src, dst),
            WorkItem::SyncFileFlags { relpath } => {
                let abs = self.abs(relpath)?;
                if let Some(input) = self.ops.sync_flags_input(relpath)? {
                    flags::sync_with_props(&abs, input)?;
                }
                Ok(())
            }
            WorkItem::PrejInstall {
                relpath,
                legacy_conflict,
            } => self
                .ops
                .write_prop_reject_file(relpath, legacy_conflict.as_ref()),
            WorkItem::DirInstall { relpath } => {
                let abs = self.abs(relpath)?;
                fs::create_dir_all(&abs).map_err(WorkQueueError::io("create directory", &abs))
            }
            WorkItem::DirRemove { relpath, recursive } => {
                remove_dir(&self.abs(relpath)?, *recursive)
            }
            WorkItem::FileCommit { relpath } => self.ops.file_commit(relpath),
            WorkItem::BaseRemove {
                relpath,
                value,
                second,
            } => {
                // Modern form: (relpath not_present_rev flag). Legacy form: (relpath keep_flag),
                // which svn only honours for WC formats <= 28; treat it as "no placeholder".
                let rev = second.and(Some(*value)).filter(|r| *r >= 0);
                self.ops.base_remove(relpath, rev)
            }
            WorkItem::RecordFileinfo { relpath, mtime } => {
                let abs = self.abs(relpath)?;
                if let Some(m) = mtime.filter(|m| *m != 0)
                    && fs::symlink_metadata(&abs).is_ok_and(|md| md.file_type().is_file())
                {
                    set_mtime(&abs, m)?;
                }
                if let Some(rec) = stat_record(relpath, &abs)? {
                    self.pending_fileinfo.push(rec);
                }
                Ok(())
            }
            WorkItem::PostUpgrade => self.ops.post_upgrade(),
        }
    }

    /// Keyword context svn builds for a node in `svn_wc__expand_keywords`.
    fn keyword_context(info: &NodeInstallInfo) -> KeywordContext {
        KeywordContext {
            rev: Some(info.changed_rev.unwrap_or(-1)),
            url: Some(format!(
                "{}/{}",
                info.repos_root_url,
                uri_encode(&info.repos_relpath)
            )),
            repos_root_url: Some(info.repos_root_url.clone()),
            date_micros: info.changed_date_micros,
            author: info.changed_author.clone(),
        }
    }

    fn translate_info(info: &NodeInstallInfo) -> TranslateInfo {
        TranslateInfo::from_props(|name| info.props.get(name).map(Vec::as_slice))
    }

    fn open_source(
        &self,
        info: &NodeInstallInfo,
        source: Option<&str>,
        relpath: &str,
    ) -> Result<Box<dyn Read>, WorkQueueError> {
        if let Some(src) = source {
            let path = self.abs(src)?;
            return Ok(Box::new(
                File::open(&path).map_err(WorkQueueError::io("open", &path))?,
            ));
        }
        let sha1 = info.checksum.as_ref().ok_or_else(|| {
            SvnError::new(
                ErrorCode::Corrupt,
                format!("can't install '{relpath}' from the pristine store: no checksum is recorded for it"),
            )
            .with_svn_code(155_017)
        })?;
        Ok(self.pristine.open_text(sha1)?)
    }

    fn file_install(
        &mut self,
        relpath: &str,
        use_commit_times: bool,
        record_fileinfo: bool,
        source: Option<&str>,
    ) -> Result<(), WorkQueueError> {
        let abs = self.abs(relpath)?;
        let info = self.ops.install_info(relpath)?;
        let mut reader = self.open_source(&info, source, relpath)?;
        let tinfo = Self::translate_info(&info);

        if tinfo.special {
            // svn bypasses eol/keywords for special files: raw bytes, then symlink or file.
            let mut bytes = Vec::new();
            reader
                .read_to_end(&mut bytes)
                .map_err(WorkQueueError::io("read source of", &abs))?;
            special::install(&abs, &bytes)?;
        } else {
            let translator = ContentTranslator::to_working(
                &tinfo,
                &Self::keyword_context(&info),
                self.config.tz,
            );
            let mtime = if use_commit_times {
                info.changed_date_micros.filter(|d| *d != 0)
            } else {
                None
            };
            let read_only =
                info.props.contains_key("svn:needs-lock") && !info.has_lock && !info.is_added;
            let executable = info.props.contains_key("svn:executable");
            let tmp_dir = self.tmp_dir()?;
            write_atomically(&abs, &tmp_dir, reader.as_mut(), translator, |tmp| {
                if let Some(m) = mtime {
                    set_mtime(tmp, m)?;
                }
                flags::set_executable(tmp, executable)?;
                if read_only {
                    flags::set_read_only(tmp)?;
                }
                Ok(())
            })?;
        }
        if record_fileinfo && let Some(rec) = stat_record(relpath, &abs)? {
            self.pending_fileinfo.push(rec);
        }
        Ok(())
    }

    fn file_translate(&mut self, local: &str, src: &str, dst: &str) -> Result<(), WorkQueueError> {
        let (src_abs, dst_abs) = (self.abs(src)?, self.abs(dst)?);
        let info = self.ops.install_info(local)?;
        let tinfo = Self::translate_info(&info);
        let mut reader = File::open(&src_abs).map_err(WorkQueueError::io("open", &src_abs))?;
        if tinfo.special {
            let mut bytes = Vec::new();
            reader
                .read_to_end(&mut bytes)
                .map_err(WorkQueueError::io("read", &src_abs))?;
            return Ok(special::install(&dst_abs, &bytes)?);
        }
        let translator =
            ContentTranslator::to_working(&tinfo, &Self::keyword_context(&info), self.config.tz);
        let tmp_dir = dst_abs
            .parent()
            .map_or_else(|| self.config.wc_root.clone(), Path::to_path_buf);
        let src_perms = fs::metadata(&src_abs).ok().map(|m| m.permissions());
        write_atomically(&dst_abs, &tmp_dir, &mut reader, translator, |tmp| {
            if let Some(p) = &src_perms {
                // Best effort, like svn_io_copy_perms.
                let _ = fs::set_permissions(tmp, p.clone());
            }
            Ok(())
        })
    }
}

/// Translates `reader` into a temp file in `tmp_dir`, lets `prepare` adjust it, and renames it
/// over `dest`.
fn write_atomically(
    dest: &Path,
    tmp_dir: &Path,
    reader: &mut dyn Read,
    translator: ContentTranslator,
    prepare: impl FnOnce(&Path) -> Result<(), WorkQueueError>,
) -> Result<(), WorkQueueError> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(WorkQueueError::io("create directory", parent))?;
    }
    let mut tmp = tempfile::Builder::new()
        .prefix("wq-")
        .tempfile_in(tmp_dir)
        .map_err(WorkQueueError::io("create temp file in", tmp_dir))?;
    translate_stream(reader, &mut tmp, translator)?;
    tmp.as_file()
        .sync_all()
        .map_err(WorkQueueError::io("sync", tmp.path()))?;
    let tmp_path = tmp.into_temp_path();
    prepare(&tmp_path)?;
    rename_replacing(&tmp_path, dest)?;
    Ok(())
}

/// `svn_io_remove_file2(path, ignore_enoent = TRUE)`.
fn remove_file_if_present(path: &Path) -> Result<(), WorkQueueError> {
    // Windows refuses to delete read-only files (needs-lock files are read-only).
    #[cfg(windows)]
    let _ = flags::set_read_write(path);
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(WorkQueueError::Io {
            op: "remove",
            path: path.to_path_buf(),
            source: e,
        }),
    }
}

/// `svn_io_file_move`: rename, falling back to copy + remove across devices. A missing source
/// means the item already ran.
fn move_file(src: &Path, dst: &Path) -> Result<(), WorkQueueError> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(WorkQueueError::io("create directory", parent))?;
    }
    match rename_replacing(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.code == ErrorCode::PathNotFound && !src.exists() => Ok(()),
        Err(_) if src.exists() => {
            // Possibly a cross-device move: copy then remove.
            fs::copy(src, dst).map_err(WorkQueueError::io("copy to", dst))?;
            remove_file_if_present(src)
        }
        Err(e) => Err(e.into()),
    }
}

/// `run_dir_remove`: recursive removal, or a non-recursive remove that tolerates a missing,
/// non-directory or non-empty target.
fn remove_dir(path: &Path, recursive: bool) -> Result<(), WorkQueueError> {
    let result = if recursive {
        fs::remove_dir_all(path)
    } else {
        fs::remove_dir(path)
    };
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e)
            if !recursive
                && (e.kind() == std::io::ErrorKind::DirectoryNotEmpty
                    || e.kind() == std::io::ErrorKind::NotADirectory) =>
        {
            Ok(())
        }
        Err(e) => Err(WorkQueueError::Io {
            op: "remove directory",
            path: path.to_path_buf(),
            source: e,
        }),
    }
}

/// Size and mtime of an installed regular file, as svn records them.
fn stat_record(relpath: &str, abs: &Path) -> Result<Option<FileInfoRecord>, WorkQueueError> {
    let md = match fs::symlink_metadata(abs) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(WorkQueueError::Io {
                op: "stat",
                path: abs.to_path_buf(),
                source: e,
            });
        }
    };
    if !md.file_type().is_file() {
        return Ok(None);
    }
    let modified = md
        .modified()
        .map_err(WorkQueueError::io("read mtime of", abs))?;
    Ok(Some(FileInfoRecord {
        relpath: relpath.to_string(),
        last_mod_time_micros: system_time_micros(modified),
        translated_size: i64::try_from(md.len()).unwrap_or(i64::MAX),
    }))
}

fn system_time_micros(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_micros()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_micros()).unwrap_or(i64::MAX),
    }
}

/// Sets a file's modification time from APR microseconds.
fn set_mtime(path: &Path, micros: i64) -> Result<(), WorkQueueError> {
    let when = if micros >= 0 {
        UNIX_EPOCH + Duration::from_micros(micros.unsigned_abs())
    } else {
        UNIX_EPOCH - Duration::from_micros(micros.unsigned_abs())
    };
    let file = open_for_times(path).map_err(WorkQueueError::io("open", path))?;
    file.set_modified(when)
        .map_err(WorkQueueError::io("set mtime of", path))
}

#[cfg(windows)]
fn open_for_times(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_WRITE_ATTRIBUTES works on read-only files, unlike generic write access.
    fs::OpenOptions::new().access_mode(0x0100).open(path)
}

#[cfg(not(windows))]
fn open_for_times(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

/// `svn_path_uri_encode`: keeps ASCII alphanumerics and `!$&'()*+,-./:=@_~`, escapes the rest as `%XX`.
fn uri_encode(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"!$&'()*+,-./:=@_~".contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}
