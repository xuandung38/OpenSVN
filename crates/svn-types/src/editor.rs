//! Synchronous delta-editor contract (the equivalent of `svn_delta_editor_t`).

use bytes::Bytes;

use crate::error::Result;
use crate::ids::RepoUrl;
use crate::ids::{DirId, FileId, Revnum};
use crate::model::{Checksum, Depth, PropChange};
use crate::rel_path::RelPath;

/// One instruction of a text-delta window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxDeltaOp {
    /// Copy `length` bytes from the source view at `offset`.
    CopyFromSource {
        /// Offset inside the source view.
        offset: u64,
        /// Number of bytes.
        length: u64,
    },
    /// Copy `length` bytes from the target view built so far at `offset` (may overlap).
    CopyFromTarget {
        /// Offset inside the target view.
        offset: u64,
        /// Number of bytes.
        length: u64,
    },
    /// Take `length` bytes from the window's new-data block at `offset`.
    NewData {
        /// Offset inside `new_data`.
        offset: u64,
        /// Number of bytes.
        length: u64,
    },
}

/// One text-delta window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxDeltaWindow {
    /// Offset of the source view in the base text.
    pub sview_offset: u64,
    /// Length of the source view.
    pub sview_len: u64,
    /// Length of the target view produced by this window.
    pub tview_len: u64,
    /// Instructions.
    pub ops: Vec<TxDeltaOp>,
    /// Literal bytes referenced by `NewData` ops.
    pub new_data: Bytes,
}

impl TxDeltaWindow {
    /// Window that rebuilds a whole file from nothing: a single `NewData` op.
    pub fn full_text(data: Bytes) -> Self {
        let len = data.len() as u64;
        Self {
            sview_offset: 0,
            sview_len: 0,
            tview_len: len,
            ops: vec![TxDeltaOp::NewData {
                offset: 0,
                length: len,
            }],
            new_data: data,
        }
    }
}

/// Receives the windows of one file's text delta.
pub trait WindowSink: Send {
    /// Delivers the next window; `None` marks the end of the delta and must be sent exactly once.
    fn window(&mut self, w: Option<&TxDeltaWindow>) -> Result<()>;
}

/// Receives the working-copy state report that precedes an update, switch or status drive.
pub trait Reporter: Send {
    /// Reports a path at a revision and depth; `start_empty` means "treat as having no children".
    fn set_path(
        &mut self,
        path: &RelPath,
        rev: Revnum,
        depth: Depth,
        start_empty: bool,
        lock_token: Option<&str>,
    ) -> Result<()>;
    /// Reports a path that is missing from the working copy.
    fn delete_path(&mut self, path: &RelPath) -> Result<()>;
    /// Reports a path that is switched to another URL.
    fn link_path(
        &mut self,
        path: &RelPath,
        url: &RepoUrl,
        rev: Revnum,
        depth: Depth,
        start_empty: bool,
        lock_token: Option<&str>,
    ) -> Result<()>;
    /// Ends the report; the server then streams the editor drive.
    fn finish(&mut self) -> Result<()>;
    /// Abandons the report.
    fn abort(&mut self) -> Result<()>;
}

/// Receives the tree changes of an update. Runs on a dedicated blocking thread and is fed
/// through a bounded command channel (see `svn_ra_api::EditorWorker`).
///
/// Ordering rules the driver must follow and every editor must accept:
/// 1. The tree is walked depth-first. Several files may be open at once (bounded), so
///    `close_file` may be delivered late, after later siblings were opened.
/// 2. `close_directory(D)` is called only after every file and directory below `D` was closed.
/// 3. `apply_textdelta` is called at most once per file, before that file's `close_file`.
/// 4. `abort_edit` may arrive at any moment (cancel or error). The editor must leave the
///    working copy in a valid, restartable (incomplete) state. After `abort_edit` or
///    `close_edit` no further call is made.
pub trait DeltaEditor: Send {
    /// Announces the revision the working copy will be at after the drive.
    fn set_target_revision(&mut self, rev: Revnum) -> Result<()>;
    /// Opens the root directory.
    fn open_root(&mut self, base_rev: Option<Revnum>) -> Result<DirId>;
    /// Deletes an entry of `parent`.
    fn delete_entry(&mut self, path: &RelPath, rev: Option<Revnum>, parent: DirId) -> Result<()>;
    /// Adds a directory, optionally copied from `(url or path, revision)`.
    fn add_directory(
        &mut self,
        path: &RelPath,
        parent: DirId,
        copy_from: Option<(&str, Revnum)>,
    ) -> Result<DirId>;
    /// Opens an existing directory.
    fn open_directory(
        &mut self,
        path: &RelPath,
        parent: DirId,
        base_rev: Option<Revnum>,
    ) -> Result<DirId>;
    /// Changes a directory property.
    fn change_dir_prop(&mut self, dir: DirId, change: PropChange) -> Result<()>;
    /// Closes a directory (rule 2).
    fn close_directory(&mut self, dir: DirId) -> Result<()>;
    /// Records a directory the user cannot see.
    fn absent_directory(&mut self, path: &RelPath, parent: DirId) -> Result<()>;
    /// Adds a file, optionally copied.
    fn add_file(
        &mut self,
        path: &RelPath,
        parent: DirId,
        copy_from: Option<(&str, Revnum)>,
    ) -> Result<FileId>;
    /// Opens an existing file.
    fn open_file(
        &mut self,
        path: &RelPath,
        parent: DirId,
        base_rev: Option<Revnum>,
    ) -> Result<FileId>;
    /// Starts the text delta of `file` and returns the sink for its windows (rule 3).
    fn apply_textdelta(
        &mut self,
        file: FileId,
        base_checksum: Option<Checksum>,
    ) -> Result<Box<dyn WindowSink>>;
    /// Changes a file property.
    fn change_file_prop(&mut self, file: FileId, change: PropChange) -> Result<()>;
    /// Closes a file, verifying the resulting text checksum when given (rule 1: may be late).
    fn close_file(&mut self, file: FileId, text_checksum: Option<Checksum>) -> Result<()>;
    /// Records a file the user cannot see.
    fn absent_file(&mut self, path: &RelPath, parent: DirId) -> Result<()>;
    /// Completes the drive successfully.
    fn close_edit(&mut self) -> Result<()>;
    /// Abandons the drive (rule 4).
    fn abort_edit(&mut self) -> Result<()>;
}
