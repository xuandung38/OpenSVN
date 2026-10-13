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
    },
    /// An entry is deleted.
    DeleteEntry {
        /// Entry.
        path: RelPath,
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
