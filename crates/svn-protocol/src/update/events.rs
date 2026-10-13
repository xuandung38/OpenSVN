//! Events produced by the update-report parser.
//!
//! The parser turns the server's nested XML into a flat, ordered event stream that mirrors the
//! delta-editor drive. Every `path` is relative to the update root and has passed
//! `RelPath::from_server`.

use bytes::Bytes;
use svn_types::{RelPath, Revnum};

use crate::caps::AcceptedHref;

/// MD5 digest as 16 bytes.
pub type Md5 = [u8; 16];
/// SHA-1 digest as 20 bytes.
pub type Sha1 = [u8; 20];

/// One step of the editor drive described by an update-report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateEvent {
    /// First event: how the server answered. With `send_all` false (skelta) file contents are
    /// never inline: the parser emits [`UpdateEvent::FetchFile`] for every file whose text was
    /// not sent, so an added file is never silently empty. `inline_props` tells whether
    /// properties are inline; when not, added nodes get [`UpdateEvent::FetchProps`].
    ReportStart {
        /// Root `send-all="true"`.
        send_all: bool,
        /// Root `inline-props="true"`.
        inline_props: bool,
    },
    /// `S:target-revision`: the revision the update brings the tree to.
    TargetRevision(Revnum),
    /// An existing directory is entered (the update root has an empty `path`).
    OpenDir {
        /// Directory.
        path: RelPath,
        /// Revision the client reported for it.
        base_rev: Option<Revnum>,
        /// `D:checked-in` href (the directory at the target revision).
        checked_in: Option<AcceptedHref>,
    },
    /// A directory is added.
    AddDir {
        /// Directory.
        path: RelPath,
        /// `D:checked-in` href.
        checked_in: Option<AcceptedHref>,
        /// Copy source (`copyfrom-path`, `copyfrom-rev`) as raw server text; the editor validates
        /// it. With a copy source, text deltas are against the source's content.
        copy_from: Option<(String, Revnum)>,
    },
    /// An existing file is entered.
    OpenFile {
        /// File.
        path: RelPath,
        /// Revision the client reported for it.
        base_rev: Option<Revnum>,
        /// `D:checked-in` href: GET it to fetch the new contents in skelta mode.
        checked_in: Option<AcceptedHref>,
    },
    /// A file is added.
    AddFile {
        /// File.
        path: RelPath,
        /// `D:checked-in` href: GET it to fetch the contents in skelta mode.
        checked_in: Option<AcceptedHref>,
        /// `sha1-checksum` attribute.
        sha1: Option<Sha1>,
        /// Copy source as raw server text; text deltas are against the source's content.
        copy_from: Option<(String, Revnum)>,
    },
    /// An entry is deleted.
    DeleteEntry {
        /// Entry.
        path: RelPath,
        /// Revision of the entry the client reported (`rev` attribute).
        rev: Option<Revnum>,
    },
    /// A directory the user cannot read.
    AbsentDir {
        /// Directory.
        path: RelPath,
    },
    /// A file the user cannot read.
    AbsentFile {
        /// File.
        path: RelPath,
    },
    /// A property is set on the node at `path`. Entry properties (`svn:entry:*`) and wc
    /// properties are delivered like any other.
    SetProp {
        /// Owning node.
        path: RelPath,
        /// Property name.
        name: String,
        /// Decoded value.
        value: Bytes,
    },
    /// A property is removed.
    RemoveProp {
        /// Owning node.
        path: RelPath,
        /// Property name.
        name: String,
    },
    /// Properties were not sent inline: fetch them (PROPFIND on `href`).
    FetchProps {
        /// Owning node.
        path: RelPath,
        /// Resource to query.
        href: Option<AcceptedHref>,
    },
    /// Contents were not sent inline: GET `href`.
    FetchFile {
        /// File.
        path: RelPath,
        /// Resource to fetch.
        href: Option<AcceptedHref>,
        /// MD5 of the base text the client must have.
        base_checksum: Option<Md5>,
    },
    /// A text delta (svndiff) starts for the file.
    TxDeltaStart {
        /// File.
        path: RelPath,
        /// MD5 of the base text.
        base_checksum: Option<Md5>,
    },
    /// Next piece of the svndiff stream (already base64-decoded).
    TxDeltaChunk {
        /// File.
        path: RelPath,
        /// Bytes.
        data: Bytes,
    },
    /// The svndiff stream of the file ended.
    TxDeltaEnd {
        /// File.
        path: RelPath,
    },
    /// A file is complete; `md5` is the checksum of the new contents when the server sent it.
    CloseFile {
        /// File.
        path: RelPath,
        /// MD5 of the new text.
        md5: Option<Md5>,
    },
    /// A directory is complete.
    CloseDir {
        /// Directory.
        path: RelPath,
    },
}

/// Checks that an event stream is a valid editor drive; usable as a debug assertion by consumers
/// and as the oracle of the fuzz target. [`UpdateParser`](super::UpdateParser) already guarantees
/// it, so this only fires on a bug.
#[derive(Debug, Default)]
pub struct DriveChecker {
    stack: Vec<(RelPath, bool)>,
    started: bool,
    root_closed: bool,
    delta: Option<RelPath>,
    delta_done: Vec<RelPath>,
}

impl DriveChecker {
    /// Creates a checker for a new report.
    pub fn new() -> Self {
        Self::default()
    }

    fn fail<T>(msg: &str) -> svn_types::Result<T> {
        Err(svn_types::SvnError::new(
            svn_types::ErrorCode::Internal,
            format!("invalid update drive: {msg}"),
        ))
    }

    fn top(&self) -> Option<&(RelPath, bool)> {
        self.stack.last()
    }

    /// Feeds the next event.
    pub fn push(&mut self, e: &UpdateEvent) -> svn_types::Result<()> {
        use UpdateEvent as E;
        if !self.started && !matches!(e, E::ReportStart { .. }) {
            return Self::fail("the first event is not ReportStart");
        }
        if self.root_closed && !matches!(e, E::ReportStart { .. }) {
            return Self::fail("event after the root closed");
        }
        if self.delta.is_some() && !matches!(e, E::TxDeltaChunk { .. } | E::TxDeltaEnd { .. }) {
            return Self::fail("event inside a text delta");
        }
        match e {
            E::ReportStart { .. } => {
                if self.started {
                    return Self::fail("second ReportStart");
                }
                self.started = true;
            }
            E::TargetRevision(_) => {
                if !self.stack.is_empty() {
                    return Self::fail("TargetRevision inside the tree");
                }
            }
            E::OpenDir { path, .. } | E::AddDir { path, .. } => {
                match self.top() {
                    None if path.is_root() && matches!(e, E::OpenDir { .. }) => {}
                    Some((_, true)) if !path.is_root() => {}
                    _ => return Self::fail("directory in the wrong place"),
                }
                self.stack.push((path.clone(), true));
            }
            E::OpenFile { path, .. } | E::AddFile { path, .. } => {
                if !matches!(self.top(), Some((_, true))) {
                    return Self::fail("file outside a directory");
                }
                self.delta_done.retain(|p| p != path);
                self.stack.push((path.clone(), false));
            }
            E::DeleteEntry { .. } | E::AbsentDir { .. } | E::AbsentFile { .. } => {
                if !matches!(self.top(), Some((_, true))) {
                    return Self::fail("entry change outside a directory");
                }
            }
            E::SetProp { path, .. } | E::RemoveProp { path, .. } | E::FetchProps { path, .. } => {
                if self.top().map(|t| &t.0) != Some(path) {
                    return Self::fail("property change for a node that is not open");
                }
            }
            E::FetchFile { path, .. } | E::TxDeltaStart { path, .. } => {
                if self.top() != Some(&(path.clone(), false)) {
                    return Self::fail("file content outside its file");
                }
                if self.delta_done.contains(path) {
                    return Self::fail("second text delta for a file");
                }
                self.delta_done.push(path.clone());
                if matches!(e, E::TxDeltaStart { .. }) {
                    self.delta = Some(path.clone());
                }
            }
            E::TxDeltaChunk { path, .. } => {
                if self.delta.as_ref() != Some(path) {
                    return Self::fail("delta chunk outside a delta");
                }
            }
            E::TxDeltaEnd { path } => {
                if self.delta.take().as_ref() != Some(path) {
                    return Self::fail("delta end without start");
                }
            }
            E::CloseFile { path, .. } => {
                if self.stack.pop() != Some((path.clone(), false)) {
                    return Self::fail("CloseFile does not match the open file");
                }
            }
            E::CloseDir { path } => {
                if self.stack.pop() != Some((path.clone(), true)) {
                    return Self::fail("CloseDir does not match the open directory");
                }
                if self.stack.is_empty() {
                    self.root_closed = true;
                }
            }
        }
        Ok(())
    }

    /// Ends the stream: everything opened must be closed.
    pub fn finish(&self) -> svn_types::Result<()> {
        if self.started && self.stack.is_empty() && self.delta.is_none() {
            Ok(())
        } else {
            Self::fail("unclosed node or delta at the end")
        }
    }
}
