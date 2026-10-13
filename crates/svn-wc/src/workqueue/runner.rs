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

use svn_types::{ErrorCode, SvnError, rename_replacing};

use crate::pristine::{PristineError, PristineIndex, PristineStore, Sha1Digest};
use crate::skel::Skel;
use crate::translate::flags::{self, SyncInput};
use crate::translate::{
    ContentTranslator, KeywordContext, TranslateInfo, UtcOffsetAt, special, translate_stream,
};

use super::paths::{resolve_operand, resolve_wc_path};
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
///
/// **Crash idempotency contract:** `next_item` may be called again with the same `completed` id
/// after a crash; deleting an already deleted item and re-storing the same file info must be
/// harmless.
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
    /// Properties used to translate the installed file: the **pristine** properties of the
    /// node (NODES top layer `properties`, as `svn_wc__db_read_node_install_info` returns).
    /// `file-translate` uses [`WorkingCopyOps::actual_props`] instead.
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
    /// The node's status is `added`. This includes copies and moves (`moved-here`), exactly
    /// like svn's `status == svn_wc__db_status_added`.
    pub is_added: bool,
    /// The WC holds a lock token for the node.
    pub has_lock: bool,
}

impl NodeInstallInfo {
    /// Keyword context svn builds for the node in `svn_wc__expand_keywords`.
    pub fn keyword_context(&self) -> KeywordContext {
        KeywordContext {
            rev: Some(self.changed_rev.unwrap_or(-1)),
            url: Some(format!(
                "{}/{}",
                self.repos_root_url,
                uri_encode(&self.repos_relpath)
            )),
            repos_root_url: Some(self.repos_root_url.clone()),
            date_micros: self.changed_date_micros,
            author: self.changed_author.clone(),
        }
    }
}

/// Database-backed operations used by work items.
///
/// **Crash idempotency contract:** a work item can run again after a crash that happened after
/// its effect but before it was marked done, so every mutating method here (`write_prop_reject_file`,
/// `file_commit`, `base_remove`, `post_upgrade`) must be safe to repeat and must converge to the
/// same state.
pub trait WorkingCopyOps {
    /// Install information for the node at `relpath` (pristine properties; see
    /// [`NodeInstallInfo::props`]).
    fn install_info(&self, relpath: &str) -> Result<NodeInstallInfo, WorkQueueError>;
    /// The node's actual (working) properties, which `file-translate` uses.
    fn actual_props(&self, relpath: &str) -> Result<BTreeMap<String, Vec<u8>>, WorkQueueError>;
    /// Inputs for `sync-file-flags`; `None` when the node is not a normal/added file.
    fn sync_flags_input(&self, relpath: &str) -> Result<Option<SyncInput>, WorkQueueError>;
    /// Writes the `.prej` property reject file recorded in the node's conflict data; the text
    /// comes from `conflicts::prej::prej_text`.
    ///
    /// `legacy` is the property conflict skel that svn 1.7 and 1.8 stored in the work item. It
    /// is not supported: an implementation must fail with a clear error telling the user to
    /// finish the interrupted operation with `svn cleanup` using the client that created the
    /// working copy, and must not silently write nothing.
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

    /// Resolves a relpath from an item below the working copy root, rejecting traversal,
    /// absolute paths, the administrative directory and symlinked ancestors.
    fn abs(&self, relpath: &str) -> Result<PathBuf, WorkQueueError> {
        resolve_wc_path(&self.config.wc_root, relpath)
    }

    /// Like [`abs`](Self::abs) but also accepts exactly `.svn/tmp/<name>`, for the operands svn
    /// documents as possibly temporary (install source, remove, move, translate source and
    /// destination).
    fn operand(&self, relpath: &str) -> Result<PathBuf, WorkQueueError> {
        resolve_operand(&self.config.wc_root, relpath, false)
    }

    /// Like [`operand`](Self::operand) for the `file-translate` source, which may also be a
    /// stored pristine text (`.svn/pristine/<xx>/<sha1>.svn-base`, as svn 1.14 and
    /// TortoiseSVN 1.14 queue them). No other operand may name a pristine.
    fn translate_source_operand(&self, relpath: &str) -> Result<PathBuf, WorkQueueError> {
        resolve_operand(&self.config.wc_root, relpath, true)
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
            WorkItem::FileRemove { relpath } => remove_file_if_present(&self.operand(relpath)?),
            WorkItem::FileMove { src, dst } => move_file(&self.operand(src)?, &self.operand(dst)?),
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
        info.keyword_context()
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
            let path = self.operand(src)?;
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
            // svn bypasses eol/keywords for special files: only the first line is buffered.
            special::install_from_reader(&abs, reader.as_mut())?;
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
        let (src_abs, dst_abs) = (self.translate_source_operand(src)?, self.operand(dst)?);
        // Keyword values come from the node's database row, but the properties that select the
        // translation are the *actual* ones (`svn_wc__get_translate_info` with props = NULL).
        let info = self.ops.install_info(local)?;
        let props = self.ops.actual_props(local)?;
        let tinfo = TranslateInfo::from_props(|name| props.get(name).map(Vec::as_slice));
        // A symlink source is special whatever the properties say (`svn_io_check_special_path`).
        let src_is_symlink = fs::symlink_metadata(&src_abs)
            .map_err(WorkQueueError::io("stat", &src_abs))?
            .file_type()
            .is_symlink();
        if src_is_symlink {
            let text = special::read_repo_normal(&src_abs)?;
            return Ok(special::install(&dst_abs, &text)?);
        }
        let mut reader = File::open(&src_abs).map_err(WorkQueueError::io("open", &src_abs))?;
        if tinfo.special {
            return Ok(special::install_from_reader(&dst_abs, &mut reader)?);
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
///
/// Windows refuses to delete read-only files (needs-lock files are read-only) and antivirus or
/// indexers hold new files briefly, so the read-only attribute is cleared and sharing violations
/// are retried with a bounded backoff (like svn's `WIN32_RETRY_LOOP`). **Unverified on Windows
/// (no Windows CI).** Elsewhere this is a single `remove_file`.
fn remove_file_if_present(path: &Path) -> Result<(), WorkQueueError> {
    let mut delay = Duration::from_millis(10);
    // Same budget as `rename_replacing` (about ten seconds), like svn's WIN32_RETRY_LOOP.
    let attempts = if cfg!(windows) { 25 } else { 1 };
    for attempt in 1..=attempts {
        #[cfg(windows)]
        let _ = flags::set_read_write(path);
        match fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) if attempt < attempts && e.kind() == std::io::ErrorKind::PermissionDenied => {
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_millis(500));
            }
            Err(e) => {
                return Err(WorkQueueError::Io {
                    op: "remove",
                    path: path.to_path_buf(),
                    source: e,
                });
            }
        }
    }
    Ok(())
}

/// `svn_io_file_move`: rename; copy + remove **only** when the rename crosses devices. A missing
/// source means the item already ran. The copy goes through a temp file in the destination
/// directory and is renamed over the destination, so a symlink at the destination is replaced,
/// never written through; a symlink source is refused.
fn move_file(src: &Path, dst: &Path) -> Result<(), WorkQueueError> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(WorkQueueError::io("create directory", parent))?;
    }
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound && fs::symlink_metadata(src).is_err() =>
        {
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => copy_then_remove(src, dst),
        // Same-device failures: let the retrying, read-only-clearing rename report or fix them.
        Err(_) => Ok(rename_replacing(src, dst)?),
    }
}

#[cfg(test)]
pub(crate) fn copy_then_remove_for_tests(src: &Path, dst: &Path) -> Result<(), WorkQueueError> {
    copy_then_remove(src, dst)
}

fn copy_then_remove(src: &Path, dst: &Path) -> Result<(), WorkQueueError> {
    let meta = fs::symlink_metadata(src).map_err(WorkQueueError::io("stat", src))?;
    if !meta.file_type().is_file() {
        return Err(WorkQueueError::UnsafePath(format!(
            "{}: refusing to copy a non-regular file across devices",
            src.display()
        )));
    }
    let dir = dst.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::Builder::new()
        .prefix(".svn-move")
        .tempfile_in(dir)
        .map_err(WorkQueueError::io("create temp file in", dir))?;
    let mut input = File::open(src).map_err(WorkQueueError::io("open", src))?;
    std::io::copy(&mut input, &mut tmp).map_err(WorkQueueError::io("copy to", tmp.path()))?;
    tmp.as_file()
        .sync_all()
        .map_err(WorkQueueError::io("sync", tmp.path()))?;
    let _ = fs::set_permissions(tmp.path(), meta.permissions());
    let tmp_path = tmp.into_temp_path();
    rename_replacing(&tmp_path, dst)?;
    remove_file_if_present(src)
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

/// Unverified on Windows (no Windows CI): relies on `FILE_WRITE_ATTRIBUTES` access (0x0100).
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
