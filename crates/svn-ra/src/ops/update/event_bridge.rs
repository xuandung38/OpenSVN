//! Translates the update-report event stream into calls on an [`EditorHandle`].
//!
//! The parser already guarantees a well-formed drive; the bridge adds the editor-side rules:
//! text deltas are decoded window by window into the editor, skeleton downloads run in parallel
//! and their `close_file` is delivered late, and `close_directory` waits for every file below it.

use bytes::Bytes;
use svn_delta::SvndiffDecoder;
use svn_protocol::update::UpdateEvent;
use svn_protocol::{HrefKind, propfind};
use svn_ra_api::{EditorHandle, WindowSender};
use svn_types::{
    CancelToken, Checksum, DirId, ErrorCode, FileId, PropChange, RelPath, Result, SvnError,
};

use super::super::read::server_svndiff;
use super::fetch_scheduler::{DownloadTask, FetchScheduler};
use super::wire::RaWire;

/// Property names derived from DAV properties when the server did not inline properties.
const ENTRY_COMMITTED_REV: &str = "svn:entry:committed-rev";
const ENTRY_COMMITTED_DATE: &str = "svn:entry:committed-date";
const ENTRY_LAST_AUTHOR: &str = "svn:entry:last-author";
const ENTRY_UUID: &str = "svn:entry:uuid";

fn corrupt(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::ProtocolViolation, msg)
}

enum Node {
    Dir(DirId),
    File(FileState),
}

struct FileState {
    id: FileId,
    /// Inline svndiff in progress.
    delta: Option<(SvndiffDecoder, WindowSender)>,
    /// Download feeding this file (skeleton mode).
    download: Option<DownloadTask>,
}

struct Frame {
    path: RelPath,
    node: Node,
}

/// A file whose download was started; `close_file` follows once it finished.
struct PendingClose {
    file: FileId,
    md5: Option<Checksum>,
    /// Number of directory frames open when the file was closed in the stream.
    dir_depth: usize,
    download: DownloadTask,
}

/// Feeds parsed events to the editor.
pub(super) struct EventBridge<'a> {
    editor: &'a EditorHandle,
    wire: &'a RaWire,
    scheduler: Option<FetchScheduler>,
    /// Status drives carry no text.
    send_text: bool,
    stack: Vec<Frame>,
    pending: Vec<PendingClose>,
    root_closed: bool,
    cancel: CancelToken,
    /// Where the server's edit root lies below the session root (the parent of a nested update
    /// target). The editor sees session-relative paths: the root and the directories down to
    /// `prefix` are opened and closed around the server's drive.
    prefix: RelPath,
    /// Directories opened for `prefix` above the server's root, outermost first.
    outer: Vec<DirId>,
}

impl<'a> EventBridge<'a> {
    pub(super) fn new(
        editor: &'a EditorHandle,
        wire: &'a RaWire,
        scheduler: Option<FetchScheduler>,
        send_text: bool,
        cancel: &CancelToken,
    ) -> Self {
        Self {
            editor,
            wire,
            scheduler,
            send_text,
            stack: Vec::new(),
            pending: Vec::new(),
            root_closed: false,
            cancel: cancel.clone(),
            prefix: RelPath::root(),
            outer: Vec::new(),
        }
    }

    /// The server's edit is rooted at `prefix` below the session root.
    pub(super) fn rooted_at(mut self, prefix: RelPath) -> Self {
        self.prefix = prefix;
        self
    }

    /// Session-relative form of a path the server sent relative to its edit root.
    fn rebase(&self, event: &mut UpdateEvent) -> Result<()> {
        use UpdateEvent as E;
        if self.prefix.is_root() {
            return Ok(());
        }
        let path = match event {
            E::OpenDir { path, .. }
            | E::AddDir { path, .. }
            | E::OpenFile { path, .. }
            | E::AddFile { path, .. }
            | E::DeleteEntry { path, .. }
            | E::AbsentDir { path }
            | E::AbsentFile { path }
            | E::SetProp { path, .. }
            | E::RemoveProp { path, .. }
            | E::FetchProps { path, .. }
            | E::FetchFile { path, .. }
            | E::TxDeltaStart { path, .. }
            | E::TxDeltaChunk { path, .. }
            | E::TxDeltaEnd { path }
            | E::CloseFile { path, .. }
            | E::CloseDir { path } => path,
            // Listed rather than matched by `_`, so a new path-bearing event cannot skip this.
            E::ReportStart { .. } | E::TargetRevision(_) => return Ok(()),
        };
        *path = if path.is_root() {
            self.prefix.clone()
        } else {
            RelPath::new(&format!("{}/{}", self.prefix, path))?
        };
        Ok(())
    }

    fn top_dir(&self) -> Result<DirId> {
        match self.stack.last() {
            Some(Frame {
                node: Node::Dir(id),
                ..
            }) => Ok(*id),
            _ => Err(corrupt("update entry outside a directory")),
        }
    }

    fn top_file(&mut self, path: &RelPath) -> Result<&mut FileState> {
        match self.stack.last_mut() {
            Some(Frame {
                path: p,
                node: Node::File(f),
            }) if p == path => Ok(f),
            _ => Err(corrupt(format!(
                "update content for {path} outside its file"
            ))),
        }
    }

    /// Handles one event.
    pub(super) async fn handle(&mut self, mut event: UpdateEvent) -> Result<()> {
        use UpdateEvent as E;
        self.rebase(&mut event)?;
        match event {
            E::ReportStart { .. } => Ok(()),
            E::TargetRevision(rev) => self.editor.set_target_revision(rev).await,
            E::OpenDir { path, base_rev, .. } => {
                let id = if self.stack.is_empty() {
                    if self.root_closed {
                        return Err(corrupt("update entry after the root was closed"));
                    }
                    // The directories down to `prefix` get the server root's `base_rev` too:
                    // the session has no working copy to read their own revisions from, and the
                    // update editor ignores `base_rev` (it reads BASE itself). These directories
                    // lie outside the target, so nothing in them is edited or bumped.
                    let mut id = self.editor.open_root(base_rev).await?;
                    let mut at = String::new();
                    for c in self.prefix.components() {
                        self.outer.push(id);
                        at = if at.is_empty() {
                            c.to_owned()
                        } else {
                            format!("{at}/{c}")
                        };
                        id = self
                            .editor
                            .open_directory(&RelPath::new(&at)?, id, base_rev)
                            .await?;
                    }
                    id
                } else {
                    let parent = self.top_dir()?;
                    self.editor.open_directory(&path, parent, base_rev).await?
                };
                self.stack.push(Frame {
                    path,
                    node: Node::Dir(id),
                });
                Ok(())
            }
            E::AddDir {
                path, copy_from, ..
            } => {
                let parent = self.top_dir()?;
                let copy = copy_from.as_ref().map(|(p, r)| (p.as_str(), *r));
                let id = self.editor.add_directory(&path, parent, copy).await?;
                self.stack.push(Frame {
                    path,
                    node: Node::Dir(id),
                });
                Ok(())
            }
            E::OpenFile { path, base_rev, .. } => {
                let parent = self.top_dir()?;
                let id = self.editor.open_file(&path, parent, base_rev).await?;
                self.push_file(path, id);
                Ok(())
            }
            E::AddFile {
                path, copy_from, ..
            } => {
                let parent = self.top_dir()?;
                let copy = copy_from.as_ref().map(|(p, r)| (p.as_str(), *r));
                let id = self.editor.add_file(&path, parent, copy).await?;
                self.push_file(path, id);
                Ok(())
            }
            E::DeleteEntry { path, rev } => {
                let parent = self.top_dir()?;
                self.editor.delete_entry(&path, rev, parent).await
            }
            E::AbsentDir { path } => {
                let parent = self.top_dir()?;
                self.editor.absent_directory(&path, parent).await
            }
            E::AbsentFile { path } => {
                let parent = self.top_dir()?;
                self.editor.absent_file(&path, parent).await
            }
            E::SetProp { path, name, value } => {
                self.prop(
                    &path,
                    PropChange {
                        name,
                        value: Some(value),
                    },
                )
                .await
            }
            E::RemoveProp { path, name } => {
                self.prop(&path, PropChange { name, value: None }).await
            }
            E::FetchProps { path, href } => self.fetch_props(&path, href).await,
            E::FetchFile {
                path,
                href,
                base_checksum,
            } => self.fetch_file(&path, href, base_checksum).await,
            E::TxDeltaStart {
                path,
                base_checksum,
            } => {
                if !self.send_text {
                    return Ok(());
                }
                let editor = self.editor;
                let file = self.top_file(&path)?;
                let sender = editor
                    .apply_textdelta(file.id, base_checksum.map(Checksum::Md5))
                    .await?;
                file.delta = Some((SvndiffDecoder::new(), sender));
                Ok(())
            }
            E::TxDeltaChunk { path, data } => {
                let file = self.top_file(&path)?;
                if let Some((decoder, sender)) = file.delta.as_mut() {
                    for w in decoder.feed(&data).map_err(server_svndiff)? {
                        sender.send(Some(w)).await?;
                    }
                }
                Ok(())
            }
            E::TxDeltaEnd { path } => {
                let file = self.top_file(&path)?;
                if let Some((mut decoder, sender)) = file.delta.take() {
                    decoder.finish().map_err(server_svndiff)?;
                    sender.send(None).await?;
                }
                Ok(())
            }
            E::CloseFile { path, md5 } => self.close_file(&path, md5).await,
            E::CloseDir { path } => self.close_dir(&path).await,
        }
    }

    fn push_file(&mut self, path: RelPath, id: FileId) {
        self.stack.push(Frame {
            path,
            node: Node::File(FileState {
                id,
                delta: None,
                download: None,
            }),
        });
    }

    async fn prop(&mut self, path: &RelPath, change: PropChange) -> Result<()> {
        match self.stack.last() {
            Some(Frame {
                path: p,
                node: Node::Dir(id),
            }) if p == path => self.editor.change_dir_prop(*id, change).await,
            Some(Frame {
                path: p,
                node: Node::File(f),
            }) if p == path => self.editor.change_file_prop(f.id, change).await,
            _ => Err(corrupt(format!(
                "property change for {path}, which is not open"
            ))),
        }
    }

    async fn fetch_file(
        &mut self,
        path: &RelPath,
        href: Option<svn_protocol::AcceptedHref>,
        base_checksum: Option<[u8; 16]>,
    ) -> Result<()> {
        if !self.send_text {
            return Ok(());
        }
        let href = href.ok_or_else(|| corrupt(format!("no href to fetch {path} from")))?;
        // Like `fetch_props`: file text comes from a node of a revision root, never from any
        // other resource the server might name (`me`, a transaction, a public URL, ...).
        if !matches!(href.kind, HrefKind::RevRoot { .. }) {
            return Err(SvnError::new(
                ErrorCode::UnsafePath,
                "fetch-file href is not a revision resource",
            ));
        }
        let scheduler = self
            .scheduler
            .clone()
            .ok_or_else(|| corrupt("server asked for file downloads in a send-all update"))?;
        // Bound the files the editor keeps open: finish the oldest download first.
        while self.pending.len() >= scheduler.max_in_flight() {
            self.complete_pending(0).await?;
        }
        let editor = self.editor;
        let file = self.top_file(path)?;
        let sender = editor
            .apply_textdelta(file.id, base_checksum.map(Checksum::Md5))
            .await?;
        file.download = Some(scheduler.spawn(href.url, sender));
        Ok(())
    }

    async fn close_file(&mut self, path: &RelPath, md5: Option<[u8; 16]>) -> Result<()> {
        let Some(Frame {
            path: p,
            node: Node::File(file),
        }) = self.stack.pop()
        else {
            return Err(corrupt(format!(
                "close of {path}, which is not an open file"
            )));
        };
        if &p != path {
            return Err(corrupt(format!("close of {path} while {p} is open")));
        }
        let md5 = md5.map(Checksum::Md5);
        match file.download {
            Some(download) => {
                self.pending.push(PendingClose {
                    file: file.id,
                    md5,
                    dir_depth: self.stack.len(),
                    download,
                });
                Ok(())
            }
            None => self.editor.close_file(file.id, md5).await,
        }
    }

    /// Waits for pending download `index` and closes its file.
    async fn complete_pending(&mut self, index: usize) -> Result<()> {
        let p = self.pending.remove(index);
        p.download.finish().await?;
        self.editor.close_file(p.file, p.md5).await
    }

    async fn close_dir(&mut self, path: &RelPath) -> Result<()> {
        let depth = self.stack.len();
        // Files below this directory are the tail of the pending list (stack discipline).
        while self.pending.last().is_some_and(|p| p.dir_depth >= depth) {
            let first = self
                .pending
                .iter()
                .position(|p| p.dir_depth >= depth)
                .unwrap_or(0);
            self.complete_pending(first).await?;
        }
        let Some(Frame {
            path: p,
            node: Node::Dir(id),
        }) = self.stack.pop()
        else {
            return Err(corrupt(format!(
                "close of {path}, which is not an open directory"
            )));
        };
        if &p != path {
            return Err(corrupt(format!("close of {path} while {p} is open")));
        }
        self.editor.close_directory(id).await?;
        if self.stack.is_empty() {
            while let Some(outer) = self.outer.pop() {
                self.editor.close_directory(outer).await?;
            }
            self.root_closed = true;
        }
        Ok(())
    }

    /// Ends the drive: everything must be closed.
    pub(super) async fn finish(&mut self) -> Result<()> {
        while !self.pending.is_empty() {
            self.complete_pending(0).await?;
        }
        if self.root_closed && self.stack.is_empty() {
            Ok(())
        } else {
            Err(corrupt("update report ended before the tree was closed"))
        }
    }

    /// Properties of a node that were not sent inline: `PROPFIND` and replay as property changes.
    async fn fetch_props(
        &mut self,
        path: &RelPath,
        href: Option<svn_protocol::AcceptedHref>,
    ) -> Result<()> {
        let href = href.ok_or_else(|| corrupt(format!("no href to fetch properties of {path}")))?;
        let HrefKind::RevRoot {
            rev,
            path: repo_path,
        } = &href.kind
        else {
            return Err(SvnError::new(
                ErrorCode::UnsafePath,
                "fetch-props href is not a revision resource",
            ));
        };
        let req = propfind::allprop_request(self.wire.urls(), *rev, repo_path)?;
        let body = self.wire.fetch_small_ok(req, &self.cancel).await?;
        let mut parser = propfind::MultiStatusParser::new();
        let entries = svn_protocol::parse_chunked(&mut parser, &body, 0)?;
        let entry = entries
            .into_iter()
            .next()
            .ok_or_else(|| corrupt("PROPFIND answered without a resource"))?;
        let mut changes: Vec<PropChange> = Vec::new();
        let text = |ns: &svn_protocol::xml::Ns, local: &str| {
            entry.get(ns, local).map(|v| v.text().trim().to_owned())
        };
        for (local, name) in [
            ("version-name", ENTRY_COMMITTED_REV),
            ("creationdate", ENTRY_COMMITTED_DATE),
            ("creator-displayname", ENTRY_LAST_AUTHOR),
        ] {
            if let Some(v) = text(&svn_protocol::xml::Ns::Dav, local) {
                changes.push(set(name, v.into_bytes()));
            }
        }
        if let Some(uuid) = &self.wire.caps().uuid {
            changes.push(set(ENTRY_UUID, uuid.clone().into_bytes()));
        }
        for (name, value) in entry.versioned_props() {
            changes.push(set(&name, value));
        }
        for c in changes {
            self.prop(path, c).await?;
        }
        Ok(())
    }
}

fn set(name: &str, value: Vec<u8>) -> PropChange {
    PropChange {
        name: name.to_owned(),
        value: Some(Bytes::from(value)),
    }
}
