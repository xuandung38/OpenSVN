//! Bridge from an async driver to a synchronous [`DeltaEditor`] running on a blocking thread.
//!
//! The driver talks to an [`EditorHandle`]; commands travel through a bounded channel (back
//! pressure keeps memory flat when the editor is slower than the network) and are executed in
//! order by the worker. The handle assigns directory and file ids itself and the worker maps
//! them to the editor's. Opening or adding a node and starting a text delta wait for the
//! editor's answer, unless the handle is [pipelined](EditorWorker::spawn_pipelined); the rest is
//! queued and its failures surface on the next call. The first failure aborts the editor once
//! and poisons the handle: every later call returns that error.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use svn_types::{
    Checksum, DeltaEditor, DirId, ErrorCode, FileId, PropChange, RelPath, Result, Revnum, SvnError,
    TxDeltaWindow, WindowSink,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Default bound of the command channel.
pub const DEFAULT_COMMAND_CAPACITY: usize = 64;

/// Answer channel of a command that returns a value.
///
/// If the worker panics while executing the command, unwinding drops this value before the
/// worker loop can record the panic. Dropping it unanswered therefore records the failure
/// itself, *before* the receiving side is woken, so the caller can never observe the closed
/// reply channel without also seeing the stored error.
struct Reply<T> {
    tx: Option<oneshot::Sender<Result<T>>>,
    failure: Failure,
}

impl<T> Reply<T> {
    fn send(mut self, r: Result<T>) {
        if let Some(tx) = self.tx.take() {
            // The driver may have gone away; there is nobody left to tell.
            let _ = tx.send(r);
        }
    }
}

impl<T> Drop for Reply<T> {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            store(&self.failure, &panic_error());
            drop(tx);
        }
    }
}
type CopyFrom = Option<(String, Revnum)>;

/// Acknowledgement of a command that creates a node or a text delta; `None` when pipelined.
type Ack = Option<Reply<()>>;

enum Command {
    SetTargetRevision(Revnum),
    OpenRoot {
        id: DirId,
        base_rev: Option<Revnum>,
        ack: Ack,
    },
    DeleteEntry {
        path: RelPath,
        rev: Option<Revnum>,
        parent: DirId,
    },
    AddDirectory {
        id: DirId,
        path: RelPath,
        parent: DirId,
        copy_from: CopyFrom,
        ack: Ack,
    },
    OpenDirectory {
        id: DirId,
        path: RelPath,
        parent: DirId,
        base_rev: Option<Revnum>,
        ack: Ack,
    },
    ChangeDirProp {
        dir: DirId,
        change: PropChange,
    },
    CloseDirectory(DirId),
    AbsentDirectory {
        path: RelPath,
        parent: DirId,
    },
    AddFile {
        id: FileId,
        path: RelPath,
        parent: DirId,
        copy_from: CopyFrom,
        ack: Ack,
    },
    OpenFile {
        id: FileId,
        path: RelPath,
        parent: DirId,
        base_rev: Option<Revnum>,
        ack: Ack,
    },
    ApplyTextDelta {
        file: FileId,
        base_checksum: Option<Checksum>,
        ack: Ack,
    },
    Window {
        file: FileId,
        window: Option<TxDeltaWindow>,
    },
    ChangeFileProp {
        file: FileId,
        change: PropChange,
    },
    CloseFile {
        file: FileId,
        text_checksum: Option<Checksum>,
    },
    AbsentFile {
        path: RelPath,
        parent: DirId,
    },
    CloseEdit {
        reply: Reply<()>,
    },
    AbortEdit {
        reply: Reply<()>,
    },
}

/// Factory for editor workers.
pub struct EditorWorker;

impl EditorWorker {
    /// Starts a blocking worker owning `editor` with a channel of `capacity` commands.
    /// Must be called inside a Tokio runtime.
    pub fn spawn(editor: Box<dyn DeltaEditor>, capacity: usize) -> EditorHandle {
        Self::start(editor, capacity, false)
    }

    /// Like [`spawn`](Self::spawn), but opening or adding a node and starting a text delta do
    /// not wait for the editor: the driver streams the whole drive and the editor works through
    /// it at its own pace (each round trip used to cost two thread switches per file). A
    /// failure of such a call surfaces on a later call, like any queued command.
    pub fn spawn_pipelined(editor: Box<dyn DeltaEditor>, capacity: usize) -> EditorHandle {
        Self::start(editor, capacity, true)
    }

    fn start(editor: Box<dyn DeltaEditor>, capacity: usize, pipelined: bool) -> EditorHandle {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        let failure = Arc::new(Mutex::new(None));
        let worker_failure = Arc::clone(&failure);
        let join = tokio::task::spawn_blocking(move || run_worker(editor, rx, &worker_failure));
        EditorHandle {
            tx,
            failure,
            join,
            next_id: AtomicU32::new(1),
            pipelined,
        }
    }
}

type Failure = Arc<Mutex<Option<SvnError>>>;

fn stored(failure: &Failure) -> Option<SvnError> {
    failure
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

fn store(failure: &Failure, err: &SvnError) {
    let mut slot = failure.lock().unwrap_or_else(PoisonError::into_inner);
    if slot.is_none() {
        *slot = Some(err.clone());
    }
}

fn finished_error() -> SvnError {
    SvnError::new(ErrorCode::Cancelled, "editor already finished")
}

fn panic_error() -> SvnError {
    SvnError::new(ErrorCode::Internal, "delta editor panicked")
}

fn answer<T>(reply: Reply<T>, r: Result<T>) {
    reply.send(r);
}

fn run_worker(
    mut editor: Box<dyn DeltaEditor>,
    mut rx: mpsc::Receiver<Command>,
    failure: &Failure,
) {
    let mut state = WorkerState::default();
    while let Some(cmd) = rx.blocking_recv() {
        let is_close = matches!(cmd, Command::CloseEdit { .. } | Command::AbortEdit { .. });
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            execute(editor.as_mut(), &mut state, cmd)
        }));
        let finished = match outcome {
            Ok(Ok(())) => is_close,
            Ok(Err(e)) => {
                store(failure, &e);
                abort_quietly(editor.as_mut());
                true
            }
            Err(_) => {
                store(failure, &panic_error());
                true // the editor is in an unknown state, do not call it again
            }
        };
        if finished {
            // Senders held by in-flight text deltas would keep the channel open forever.
            // Close it so their sends fail fast, and answer whatever is still queued.
            rx.close();
            let err = stored(failure).unwrap_or_else(finished_error);
            while let Ok(cmd) = rx.try_recv() {
                reject(cmd, &err);
            }
            return;
        }
    }
    // Every sender is gone without close/abort: leave the working copy restartable.
    abort_quietly(editor.as_mut());
}

fn abort_quietly(editor: &mut dyn DeltaEditor) {
    let _ = catch_unwind(AssertUnwindSafe(|| editor.abort_edit()));
}

fn reject(cmd: Command, err: &SvnError) {
    match cmd {
        Command::OpenRoot { ack, .. }
        | Command::AddDirectory { ack, .. }
        | Command::OpenDirectory { ack, .. }
        | Command::AddFile { ack, .. }
        | Command::OpenFile { ack, .. }
        | Command::ApplyTextDelta { ack, .. } => {
            if let Some(ack) = ack {
                answer(ack, Err(err.clone()));
            }
        }
        Command::CloseEdit { reply } => answer(reply, Err(err.clone())),
        Command::AbortEdit { reply } => answer(reply, Ok(())),
        _ => {}
    }
}

/// The worker's view of the drive: handle ids to editor ids, and the open text deltas.
#[derive(Default)]
struct WorkerState {
    dirs: HashMap<u32, DirId>,
    files: HashMap<u32, FileId>,
    sinks: HashMap<u32, Box<dyn WindowSink>>,
}

fn unknown(what: &str) -> SvnError {
    SvnError::new(ErrorCode::Internal, format!("unknown {what} handle"))
}

impl WorkerState {
    fn dir(&self, id: DirId) -> Result<DirId> {
        self.dirs
            .get(&id.0)
            .copied()
            .ok_or_else(|| unknown("directory"))
    }

    fn file(&self, id: FileId) -> Result<FileId> {
        self.files
            .get(&id.0)
            .copied()
            .ok_or_else(|| unknown("file"))
    }
}

/// Runs one command; acknowledged commands answer here and report `Ok` to the loop unless
/// the editor call itself failed (the loop then aborts the editor).
fn execute(ed: &mut dyn DeltaEditor, st: &mut WorkerState, cmd: Command) -> Result<()> {
    fn relay<T>(reply: Reply<T>, r: Result<T>) -> Result<()> {
        let status = r.as_ref().map(|_| ()).map_err(Clone::clone);
        answer(reply, r);
        status
    }
    /// Answers the acknowledgement (if any) with the outcome of `r`.
    fn acked(ack: Ack, r: Result<()>) -> Result<()> {
        match ack {
            Some(ack) => relay(ack, r),
            None => r,
        }
    }
    fn cf(c: &CopyFrom) -> Option<(&str, Revnum)> {
        c.as_ref().map(|(p, r)| (p.as_str(), *r))
    }
    match cmd {
        Command::SetTargetRevision(rev) => ed.set_target_revision(rev),
        Command::OpenRoot { id, base_rev, ack } => {
            let r = ed.open_root(base_rev).map(|d| {
                st.dirs.insert(id.0, d);
            });
            acked(ack, r)
        }
        Command::DeleteEntry { path, rev, parent } => ed.delete_entry(&path, rev, st.dir(parent)?),
        Command::AddDirectory {
            id,
            path,
            parent,
            copy_from,
            ack,
        } => {
            let r = st
                .dir(parent)
                .and_then(|p| ed.add_directory(&path, p, cf(&copy_from)))
                .map(|d| {
                    st.dirs.insert(id.0, d);
                });
            acked(ack, r)
        }
        Command::OpenDirectory {
            id,
            path,
            parent,
            base_rev,
            ack,
        } => {
            let r = st
                .dir(parent)
                .and_then(|p| ed.open_directory(&path, p, base_rev))
                .map(|d| {
                    st.dirs.insert(id.0, d);
                });
            acked(ack, r)
        }
        Command::ChangeDirProp { dir, change } => ed.change_dir_prop(st.dir(dir)?, change),
        Command::CloseDirectory(dir) => {
            let d = st.dir(dir)?;
            st.dirs.remove(&dir.0);
            ed.close_directory(d)
        }
        Command::AbsentDirectory { path, parent } => ed.absent_directory(&path, st.dir(parent)?),
        Command::AddFile {
            id,
            path,
            parent,
            copy_from,
            ack,
        } => {
            let r = st
                .dir(parent)
                .and_then(|p| ed.add_file(&path, p, cf(&copy_from)))
                .map(|f| {
                    st.files.insert(id.0, f);
                });
            acked(ack, r)
        }
        Command::OpenFile {
            id,
            path,
            parent,
            base_rev,
            ack,
        } => {
            let r = st
                .dir(parent)
                .and_then(|p| ed.open_file(&path, p, base_rev))
                .map(|f| {
                    st.files.insert(id.0, f);
                });
            acked(ack, r)
        }
        Command::ApplyTextDelta {
            file,
            base_checksum,
            ack,
        } => {
            let r = st
                .file(file)
                .and_then(|f| ed.apply_textdelta(f, base_checksum))
                .map(|sink| {
                    st.sinks.insert(file.0, sink);
                });
            acked(ack, r)
        }
        Command::Window { file, window } => {
            let end = window.is_none();
            let sink = st.sinks.get_mut(&file.0).ok_or_else(|| {
                SvnError::new(
                    ErrorCode::Internal,
                    "window for a file without a text delta",
                )
            })?;
            let r = sink.window(window.as_ref());
            if end {
                st.sinks.remove(&file.0);
            }
            r
        }
        Command::ChangeFileProp { file, change } => ed.change_file_prop(st.file(file)?, change),
        Command::CloseFile {
            file,
            text_checksum,
        } => {
            let f = st.file(file)?;
            st.files.remove(&file.0);
            ed.close_file(f, text_checksum)
        }
        Command::AbsentFile { path, parent } => ed.absent_file(&path, st.dir(parent)?),
        Command::CloseEdit { reply } => relay(reply, ed.close_edit()),
        Command::AbortEdit { reply } => relay(reply, ed.abort_edit()),
    }
}

/// Async front of a running editor worker. Dropping it without `close_edit`/`abort_edit`
/// makes the worker abort the editor.
pub struct EditorHandle {
    tx: mpsc::Sender<Command>,
    failure: Failure,
    join: JoinHandle<()>,
    /// Next directory or file id handed out (ids are unique across both kinds).
    next_id: AtomicU32,
    /// Opening, adding and text deltas do not wait for the editor.
    pipelined: bool,
}

/// Feeds the windows of one file's text delta; obtained from [`EditorHandle::apply_textdelta`].
pub struct WindowSender {
    file: FileId,
    tx: mpsc::Sender<Command>,
    failure: Failure,
}

fn gone(failure: &Failure) -> SvnError {
    stored(failure).unwrap_or_else(finished_error)
}

async fn send(tx: &mpsc::Sender<Command>, failure: &Failure, cmd: Command) -> Result<()> {
    if let Some(e) = stored(failure) {
        return Err(e);
    }
    tx.send(cmd).await.map_err(|_| gone(failure))
}

impl WindowSender {
    /// Queues the next window; `None` ends the delta (exactly once).
    pub async fn send(&self, window: Option<TxDeltaWindow>) -> Result<()> {
        send(
            &self.tx,
            &self.failure,
            Command::Window {
                file: self.file,
                window,
            },
        )
        .await
    }
}

impl EditorHandle {
    async fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        let reply = Reply {
            tx: Some(tx),
            failure: Arc::clone(&self.failure),
        };
        send(&self.tx, &self.failure, make(reply)).await?;
        rx.await.map_err(|_| gone(&self.failure))?
    }

    async fn push(&self, cmd: Command) -> Result<()> {
        send(&self.tx, &self.failure, cmd).await
    }

    /// See [`DeltaEditor::set_target_revision`].
    pub async fn set_target_revision(&self, rev: Revnum) -> Result<()> {
        self.push(Command::SetTargetRevision(rev)).await
    }
    fn alloc(&self) -> u32 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Sends a command that creates a node or a text delta: waits for the editor's answer
    /// unless pipelined.
    async fn acked(&self, make: impl FnOnce(Ack) -> Command) -> Result<()> {
        if self.pipelined {
            self.push(make(None)).await
        } else {
            self.call(|reply| make(Some(reply))).await
        }
    }

    /// See [`DeltaEditor::open_root`].
    pub async fn open_root(&self, base_rev: Option<Revnum>) -> Result<DirId> {
        let id = DirId(self.alloc());
        self.acked(|ack| Command::OpenRoot { id, base_rev, ack })
            .await?;
        Ok(id)
    }
    /// See [`DeltaEditor::delete_entry`].
    pub async fn delete_entry(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        parent: DirId,
    ) -> Result<()> {
        self.push(Command::DeleteEntry {
            path: path.clone(),
            rev,
            parent,
        })
        .await
    }
    /// See [`DeltaEditor::add_directory`].
    pub async fn add_directory(
        &self,
        path: &RelPath,
        parent: DirId,
        copy_from: Option<(&str, Revnum)>,
    ) -> Result<DirId> {
        let copy_from = copy_from.map(|(p, r)| (p.to_owned(), r));
        let id = DirId(self.alloc());
        self.acked(|ack| Command::AddDirectory {
            id,
            path: path.clone(),
            parent,
            copy_from,
            ack,
        })
        .await?;
        Ok(id)
    }
    /// See [`DeltaEditor::open_directory`].
    pub async fn open_directory(
        &self,
        path: &RelPath,
        parent: DirId,
        base_rev: Option<Revnum>,
    ) -> Result<DirId> {
        let id = DirId(self.alloc());
        self.acked(|ack| Command::OpenDirectory {
            id,
            path: path.clone(),
            parent,
            base_rev,
            ack,
        })
        .await?;
        Ok(id)
    }
    /// See [`DeltaEditor::change_dir_prop`].
    pub async fn change_dir_prop(&self, dir: DirId, change: PropChange) -> Result<()> {
        self.push(Command::ChangeDirProp { dir, change }).await
    }
    /// See [`DeltaEditor::close_directory`].
    pub async fn close_directory(&self, dir: DirId) -> Result<()> {
        self.push(Command::CloseDirectory(dir)).await
    }
    /// See [`DeltaEditor::absent_directory`].
    pub async fn absent_directory(&self, path: &RelPath, parent: DirId) -> Result<()> {
        self.push(Command::AbsentDirectory {
            path: path.clone(),
            parent,
        })
        .await
    }
    /// See [`DeltaEditor::add_file`].
    pub async fn add_file(
        &self,
        path: &RelPath,
        parent: DirId,
        copy_from: Option<(&str, Revnum)>,
    ) -> Result<FileId> {
        let copy_from = copy_from.map(|(p, r)| (p.to_owned(), r));
        let id = FileId(self.alloc());
        self.acked(|ack| Command::AddFile {
            id,
            path: path.clone(),
            parent,
            copy_from,
            ack,
        })
        .await?;
        Ok(id)
    }
    /// See [`DeltaEditor::open_file`].
    pub async fn open_file(
        &self,
        path: &RelPath,
        parent: DirId,
        base_rev: Option<Revnum>,
    ) -> Result<FileId> {
        let id = FileId(self.alloc());
        self.acked(|ack| Command::OpenFile {
            id,
            path: path.clone(),
            parent,
            base_rev,
            ack,
        })
        .await?;
        Ok(id)
    }
    /// See [`DeltaEditor::apply_textdelta`]; windows are then pushed through the returned sender.
    pub async fn apply_textdelta(
        &self,
        file: FileId,
        base_checksum: Option<Checksum>,
    ) -> Result<WindowSender> {
        self.acked(|ack| Command::ApplyTextDelta {
            file,
            base_checksum,
            ack,
        })
        .await?;
        Ok(WindowSender {
            file,
            tx: self.tx.clone(),
            failure: Arc::clone(&self.failure),
        })
    }
    /// See [`DeltaEditor::change_file_prop`].
    pub async fn change_file_prop(&self, file: FileId, change: PropChange) -> Result<()> {
        self.push(Command::ChangeFileProp { file, change }).await
    }
    /// See [`DeltaEditor::close_file`]; may be called after later siblings were opened.
    pub async fn close_file(&self, file: FileId, text_checksum: Option<Checksum>) -> Result<()> {
        self.push(Command::CloseFile {
            file,
            text_checksum,
        })
        .await
    }
    /// See [`DeltaEditor::absent_file`].
    pub async fn absent_file(&self, path: &RelPath, parent: DirId) -> Result<()> {
        self.push(Command::AbsentFile {
            path: path.clone(),
            parent,
        })
        .await
    }

    /// Completes the drive: waits for every queued command, then for the worker to exit.
    /// Returns the first queued failure if there was one.
    pub async fn close_edit(self) -> Result<()> {
        let r = self.call(|reply| Command::CloseEdit { reply }).await;
        self.shutdown(r).await
    }

    /// Aborts the drive from any state and waits for the worker to exit. Commands still queued
    /// are executed first; the editor's own error (if any) is returned.
    pub async fn abort_edit(self) -> Result<()> {
        let r = self.call(|reply| Command::AbortEdit { reply }).await;
        // A poisoned handle already aborted the editor; that is not an error for the caller.
        let r = match r {
            Err(e)
                if stored(&self.failure)
                    .is_some_and(|f| f.code == e.code && f.message == e.message) =>
            {
                Ok(())
            }
            other => other,
        };
        self.shutdown(r).await
    }

    async fn shutdown(self, result: Result<()>) -> Result<()> {
        let Self { tx, join, .. } = self;
        drop(tx);
        match join.await {
            Ok(()) => result,
            Err(e) => Err(SvnError::new(
                ErrorCode::Internal,
                format!("editor worker failed: {e}"),
            )),
        }
    }
}
