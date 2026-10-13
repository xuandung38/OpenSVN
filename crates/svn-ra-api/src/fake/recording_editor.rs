//! Delta editor that records every call and enforces the documented ordering rules.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use svn_types::{
    Checksum, DeltaEditor, DirId, ErrorCode, FileId, PropChange, RelPath, Result, Revnum, SvnError,
    TxDeltaOp, TxDeltaWindow, WindowSink,
};

/// One recorded editor call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorEvent {
    /// `set_target_revision`.
    SetTargetRevision(Revnum),
    /// `open_root` returning the id.
    OpenRoot(DirId),
    /// `delete_entry`.
    DeleteEntry(RelPath),
    /// `add_directory`.
    AddDirectory(RelPath, DirId),
    /// `open_directory`.
    OpenDirectory(RelPath, DirId),
    /// `change_dir_prop`.
    ChangeDirProp(DirId, String),
    /// `close_directory`.
    CloseDirectory(DirId),
    /// `absent_directory`.
    AbsentDirectory(RelPath),
    /// `add_file`.
    AddFile(RelPath, FileId),
    /// `open_file`.
    OpenFile(RelPath, FileId),
    /// `apply_textdelta`.
    ApplyTextDelta(FileId),
    /// A window arrived (`None` = end of delta) carrying `new_data` bytes.
    Window(FileId, Option<usize>),
    /// `change_file_prop`.
    ChangeFileProp(FileId, String),
    /// `close_file`.
    CloseFile(FileId),
    /// `absent_file`.
    AbsentFile(RelPath),
    /// `close_edit`.
    CloseEdit,
    /// `abort_edit`.
    AbortEdit,
}

#[derive(Debug)]
struct DirState {
    path: RelPath,
    open: bool,
}

#[derive(Debug)]
struct FileState {
    path: RelPath,
    parent: DirId,
    open: bool,
    delta_started: bool,
    delta_done: bool,
}

#[derive(Default)]
struct Shared {
    events: Vec<EditorEvent>,
    violations: Vec<String>,
    dirs: HashMap<DirId, DirState>,
    files: HashMap<FileId, FileState>,
    /// Open directories, innermost last (depth-first walk).
    stack: Vec<DirId>,
    contents: BTreeMap<RelPath, Vec<u8>>,
    next_id: u32,
    root_opened: bool,
    closed: bool,
    aborted: bool,
    /// Number of calls seen so far (windows included).
    calls: usize,
    fail_at: Option<usize>,
    hook: Option<Box<dyn Fn(usize) + Send>>,
}

/// Read access to what a [`RecordingEditor`] saw; stays valid after the editor moved into a worker.
#[derive(Clone, Default)]
pub struct RecordingLog(Arc<Mutex<Shared>>);

impl RecordingLog {
    fn g(&self) -> MutexGuard<'_, Shared> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// All calls in arrival order.
    pub fn events(&self) -> Vec<EditorEvent> {
        self.g().events.clone()
    }

    /// Ordering-rule violations observed (empty when the driver behaved).
    pub fn violations(&self) -> Vec<String> {
        self.g().violations.clone()
    }

    /// Text assembled from the windows (assuming an empty base), by path.
    pub fn file_contents(&self) -> BTreeMap<RelPath, Vec<u8>> {
        self.g().contents.clone()
    }

    /// `close_edit` was called successfully.
    pub fn is_closed(&self) -> bool {
        self.g().closed
    }

    /// `abort_edit` was called.
    pub fn is_aborted(&self) -> bool {
        self.g().aborted
    }
}

/// Editor that records calls and rejects drives that break the ordering rules (documented on
/// [`DeltaEditor`]). A rejected call returns a `Corrupt` error and is listed in
/// [`RecordingLog::violations`].
pub struct RecordingEditor {
    log: RecordingLog,
}

impl RecordingEditor {
    /// Creates an editor and the log handle to inspect it later.
    pub fn new() -> (Self, RecordingLog) {
        let log = RecordingLog::default();
        (Self { log: log.clone() }, log)
    }

    fn g(&self) -> MutexGuard<'_, Shared> {
        self.log.g()
    }

    /// Makes the `n`-th call (1-based, windows count) fail with an `Internal` error.
    #[must_use]
    pub fn fail_at(self, n: usize) -> Self {
        self.g().fail_at = Some(n);
        self
    }

    /// Runs `hook(call_number)` at the start of every call; lets a test cancel mid-drive.
    #[must_use]
    pub fn on_call(self, hook: impl Fn(usize) + Send + 'static) -> Self {
        self.g().hook = Some(Box::new(hook));
        self
    }
}

fn violation(s: &mut Shared, msg: String) -> SvnError {
    s.violations.push(msg.clone());
    SvnError::new(
        ErrorCode::Corrupt,
        format!("editor ordering violation: {msg}"),
    )
}

/// Counts the call, runs the test hook and applies injected failures.
fn count_call(s: &mut Shared) -> Result<()> {
    s.calls += 1;
    if let Some(h) = &s.hook {
        h(s.calls);
    }
    if s.fail_at == Some(s.calls) {
        return Err(SvnError::new(
            ErrorCode::Internal,
            "injected editor failure",
        ));
    }
    Ok(())
}

/// Rejects calls after the edit ended, then counts the call.
fn live(s: &mut Shared, call: &str) -> Result<()> {
    if s.closed || s.aborted {
        return Err(violation(s, format!("{call} after the edit ended")));
    }
    count_call(s)
}

/// A new child may only be created under the innermost open directory (depth-first walk).
fn check_parent(s: &mut Shared, parent: DirId, call: &str) -> Result<()> {
    if s.stack.last() != Some(&parent) {
        return Err(violation(
            s,
            format!("{call} under {parent:?}, which is not the innermost open directory"),
        ));
    }
    Ok(())
}

fn new_id(s: &mut Shared) -> u32 {
    s.next_id += 1;
    s.next_id
}

fn open_dir(s: &mut Shared, call: &str, path: &RelPath, parent: DirId) -> Result<DirId> {
    live(s, call)?;
    check_parent(s, parent, call)?;
    let id = DirId(new_id(s));
    s.dirs.insert(
        id,
        DirState {
            path: path.clone(),
            open: true,
        },
    );
    s.stack.push(id);
    Ok(id)
}

fn open_file(s: &mut Shared, call: &str, path: &RelPath, parent: DirId) -> Result<FileId> {
    live(s, call)?;
    check_parent(s, parent, call)?;
    let id = FileId(new_id(s));
    s.files.insert(
        id,
        FileState {
            path: path.clone(),
            parent,
            open: true,
            delta_started: false,
            delta_done: false,
        },
    );
    s.contents.insert(path.clone(), Vec::new());
    Ok(id)
}

fn open_file_state<'a>(s: &'a mut Shared, f: FileId, call: &str) -> Result<&'a mut FileState> {
    if s.closed || s.aborted {
        return Err(violation(s, format!("{call} after the edit ended")));
    }
    count_call(s)?;
    match s.files.get(&f) {
        Some(st) if st.open => {}
        _ => {
            return Err(violation(
                s,
                format!("{call} on a file that is not open ({f:?})"),
            ));
        }
    }
    s.files
        .get_mut(&f)
        .ok_or_else(|| SvnError::new(ErrorCode::Internal, "file vanished"))
}

fn open_dir_state(s: &mut Shared, d: DirId, call: &str) -> Result<()> {
    live(s, call)?;
    if !s.dirs.get(&d).is_some_and(|x| x.open) {
        return Err(violation(
            s,
            format!("{call} on a directory that is not open ({d:?})"),
        ));
    }
    Ok(())
}

impl DeltaEditor for RecordingEditor {
    fn set_target_revision(&mut self, rev: Revnum) -> Result<()> {
        let mut s = self.g();
        live(&mut s, "set_target_revision")?;
        s.events.push(EditorEvent::SetTargetRevision(rev));
        Ok(())
    }

    fn open_root(&mut self, _base_rev: Option<Revnum>) -> Result<DirId> {
        let mut s = self.g();
        live(&mut s, "open_root")?;
        if s.root_opened {
            return Err(violation(&mut s, "open_root called twice".into()));
        }
        s.root_opened = true;
        let id = DirId(new_id(&mut s));
        s.dirs.insert(
            id,
            DirState {
                path: RelPath::root(),
                open: true,
            },
        );
        s.stack.push(id);
        s.events.push(EditorEvent::OpenRoot(id));
        Ok(id)
    }

    fn delete_entry(&mut self, path: &RelPath, _rev: Option<Revnum>, parent: DirId) -> Result<()> {
        let mut s = self.g();
        live(&mut s, "delete_entry")?;
        check_parent(&mut s, parent, "delete_entry")?;
        s.contents.retain(|k, _| !k.starts_with(path));
        s.events.push(EditorEvent::DeleteEntry(path.clone()));
        Ok(())
    }

    fn add_directory(
        &mut self,
        path: &RelPath,
        parent: DirId,
        _copy_from: Option<(&str, Revnum)>,
    ) -> Result<DirId> {
        let mut s = self.g();
        let id = open_dir(&mut s, "add_directory", path, parent)?;
        s.events.push(EditorEvent::AddDirectory(path.clone(), id));
        Ok(id)
    }

    fn open_directory(
        &mut self,
        path: &RelPath,
        parent: DirId,
        _base_rev: Option<Revnum>,
    ) -> Result<DirId> {
        let mut s = self.g();
        let id = open_dir(&mut s, "open_directory", path, parent)?;
        s.events.push(EditorEvent::OpenDirectory(path.clone(), id));
        Ok(id)
    }

    fn change_dir_prop(&mut self, dir: DirId, change: PropChange) -> Result<()> {
        let mut s = self.g();
        open_dir_state(&mut s, dir, "change_dir_prop")?;
        s.events.push(EditorEvent::ChangeDirProp(dir, change.name));
        Ok(())
    }

    fn close_directory(&mut self, dir: DirId) -> Result<()> {
        let mut s = self.g();
        open_dir_state(&mut s, dir, "close_directory")?;
        if s.stack.last() != Some(&dir) {
            return Err(violation(
                &mut s,
                format!("close_directory({dir:?}) while a child directory is still open"),
            ));
        }
        if let Some((fid, _)) = s.files.iter().find(|(_, f)| f.open && f.parent == dir) {
            let fid = *fid;
            return Err(violation(
                &mut s,
                format!("close_directory({dir:?}) before file {fid:?} was closed"),
            ));
        }
        let path = s
            .dirs
            .get(&dir)
            .map(|d| d.path.clone())
            .unwrap_or_else(RelPath::root);
        if let Some((fid, _)) = s
            .files
            .iter()
            .find(|(_, f)| f.open && f.path.starts_with(&path) && !path.is_root())
        {
            let fid = *fid;
            return Err(violation(
                &mut s,
                format!("close_directory({dir:?}) before descendant file {fid:?} was closed"),
            ));
        }
        s.stack.pop();
        if let Some(d) = s.dirs.get_mut(&dir) {
            d.open = false;
        }
        s.events.push(EditorEvent::CloseDirectory(dir));
        Ok(())
    }

    fn absent_directory(&mut self, path: &RelPath, parent: DirId) -> Result<()> {
        let mut s = self.g();
        live(&mut s, "absent_directory")?;
        check_parent(&mut s, parent, "absent_directory")?;
        s.events.push(EditorEvent::AbsentDirectory(path.clone()));
        Ok(())
    }

    fn add_file(
        &mut self,
        path: &RelPath,
        parent: DirId,
        _copy_from: Option<(&str, Revnum)>,
    ) -> Result<FileId> {
        let mut s = self.g();
        let id = open_file(&mut s, "add_file", path, parent)?;
        s.events.push(EditorEvent::AddFile(path.clone(), id));
        Ok(id)
    }

    fn open_file(
        &mut self,
        path: &RelPath,
        parent: DirId,
        _base_rev: Option<Revnum>,
    ) -> Result<FileId> {
        let mut s = self.g();
        let id = open_file(&mut s, "open_file", path, parent)?;
        s.events.push(EditorEvent::OpenFile(path.clone(), id));
        Ok(id)
    }

    fn apply_textdelta(
        &mut self,
        file: FileId,
        _base_checksum: Option<Checksum>,
    ) -> Result<Box<dyn WindowSink>> {
        let mut s = self.g();
        let st = open_file_state(&mut s, file, "apply_textdelta")?;
        if st.delta_started {
            return Err(violation(
                &mut s,
                format!("apply_textdelta called twice for {file:?}"),
            ));
        }
        st.delta_started = true;
        s.events.push(EditorEvent::ApplyTextDelta(file));
        Ok(Box::new(RecordingSink {
            file,
            log: self.log.clone(),
        }))
    }

    fn change_file_prop(&mut self, file: FileId, change: PropChange) -> Result<()> {
        let mut s = self.g();
        open_file_state(&mut s, file, "change_file_prop")?;
        s.events
            .push(EditorEvent::ChangeFileProp(file, change.name));
        Ok(())
    }

    fn close_file(&mut self, file: FileId, _text_checksum: Option<Checksum>) -> Result<()> {
        let mut s = self.g();
        let st = open_file_state(&mut s, file, "close_file")?;
        if st.delta_started && !st.delta_done {
            return Err(violation(
                &mut s,
                format!("close_file({file:?}) before its text delta ended"),
            ));
        }
        st.open = false;
        s.events.push(EditorEvent::CloseFile(file));
        Ok(())
    }

    fn absent_file(&mut self, path: &RelPath, parent: DirId) -> Result<()> {
        let mut s = self.g();
        live(&mut s, "absent_file")?;
        check_parent(&mut s, parent, "absent_file")?;
        s.events.push(EditorEvent::AbsentFile(path.clone()));
        Ok(())
    }

    fn close_edit(&mut self) -> Result<()> {
        let mut s = self.g();
        live(&mut s, "close_edit")?;
        if let Some(d) = s.stack.last().copied() {
            return Err(violation(
                &mut s,
                format!("close_edit with directory {d:?} still open"),
            ));
        }
        if let Some(f) = s.files.iter().find(|(_, f)| f.open).map(|(id, _)| *id) {
            return Err(violation(
                &mut s,
                format!("close_edit with file {f:?} still open"),
            ));
        }
        s.closed = true;
        s.events.push(EditorEvent::CloseEdit);
        Ok(())
    }

    fn abort_edit(&mut self) -> Result<()> {
        let mut s = self.g();
        if s.closed {
            return Err(violation(&mut s, "abort_edit after close_edit".into()));
        }
        // Allowed at any moment, and repeatable.
        s.aborted = true;
        s.events.push(EditorEvent::AbortEdit);
        Ok(())
    }
}

struct RecordingSink {
    file: FileId,
    log: RecordingLog,
}

impl WindowSink for RecordingSink {
    fn window(&mut self, w: Option<&TxDeltaWindow>) -> Result<()> {
        let mut s = self.log.g();
        let file = self.file;
        let st = open_file_state(&mut s, file, "window")?;
        if st.delta_done {
            return Err(violation(
                &mut s,
                format!("window after the end of the delta of {file:?}"),
            ));
        }
        let path = st.path.clone();
        let Some(w) = w else {
            st.delta_done = true;
            s.events.push(EditorEvent::Window(file, None));
            return Ok(());
        };
        let out = s.contents.entry(path).or_default();
        let mut bad = None;
        let start = out.len();
        for op in &w.ops {
            match *op {
                TxDeltaOp::NewData { offset, length } => {
                    match w.new_data.get(offset as usize..(offset + length) as usize) {
                        Some(d) => out.extend_from_slice(d),
                        None => bad = Some("NewData range outside new_data"),
                    }
                }
                TxDeltaOp::CopyFromTarget { offset, length } => {
                    for i in 0..length as usize {
                        match out.get(start + offset as usize + i).copied() {
                            Some(b) => out.push(b),
                            None => bad = Some("CopyFromTarget range outside the target view"),
                        }
                    }
                }
                TxDeltaOp::CopyFromSource { .. } => bad = Some("CopyFromSource needs a base text"),
            }
        }
        if let Some(m) = bad {
            return Err(violation(&mut s, m.into()));
        }
        s.events
            .push(EditorEvent::Window(file, Some(w.new_data.len())));
        Ok(())
    }
}
