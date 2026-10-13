//! Plain data types exchanged between engine layers.

use bytes::Bytes;

use crate::display_path::RepoDisplayPath;
use crate::ids::{Revnum, Timestamp};
use crate::rel_path::RelPath;

/// Kind of a repository or working-copy node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    /// No node.
    None,
    /// Regular file.
    File,
    /// Directory.
    Dir,
    /// Symbolic link.
    Symlink,
    /// Present but of unknown kind.
    Unknown,
}

/// Requested or recorded checkout depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Depth {
    /// Only the directory itself.
    Empty,
    /// Directory and its file children.
    Files,
    /// Directory and all immediate children.
    Immediates,
    /// Whole subtree.
    Infinity,
    /// Exclude from the working copy.
    Exclude,
    /// Not known.
    Unknown,
}

/// Content checksum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Checksum {
    /// MD5 digest (wire-level text-delta checksums).
    Md5([u8; 16]),
    /// SHA-1 digest (pristine store key).
    Sha1([u8; 20]),
}

/// Property set or deletion (`value == None` deletes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropChange {
    /// Property name.
    pub name: String,
    /// New value, or `None` to delete.
    pub value: Option<Bytes>,
}

/// Repository lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockInfo {
    /// Locked repository path, display only (convert explicitly with `RelPath::from_server`).
    pub path: RepoDisplayPath,
    /// Lock token.
    pub token: String,
    /// Lock owner.
    pub owner: String,
    /// Optional comment.
    pub comment: Option<String>,
    /// Creation time.
    pub created: Timestamp,
    /// Expiry time, if any.
    pub expires: Option<Timestamp>,
}

/// Directory listing entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// Entry name (single path segment). Server text: validate with `RelPath::from_server`
    /// before using it to build a local path.
    pub name: String,
    /// Node kind.
    pub kind: NodeKind,
    /// File size in bytes (files only).
    pub size: Option<u64>,
    /// Revision of last change.
    pub created_rev: Option<Revnum>,
    /// Author of last change.
    pub last_author: Option<String>,
    /// Time of last change.
    pub time: Option<Timestamp>,
    /// Whether the node has properties.
    pub has_props: bool,
    /// Lock, if listed with locks.
    pub lock: Option<LockInfo>,
}

/// Action recorded for a changed path in a revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeAction {
    /// Added.
    Added,
    /// Modified.
    Modified,
    /// Deleted.
    Deleted,
    /// Replaced.
    Replaced,
}

/// One changed path of a log entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedPath {
    /// Absolute repository path, display only (convert explicitly with `RelPath::from_server`).
    pub path: RepoDisplayPath,
    /// What happened.
    pub action: ChangeAction,
    /// Node kind.
    pub kind: NodeKind,
    /// Copy source path (display only, see `path`) and revision.
    pub copy_from: Option<(RepoDisplayPath, Revnum)>,
    /// Whether file text changed.
    pub text_mods: bool,
    /// Whether properties changed.
    pub prop_mods: bool,
}

/// One log entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// Revision.
    pub rev: Revnum,
    /// Author.
    pub author: Option<String>,
    /// Commit time.
    pub date: Option<Timestamp>,
    /// Commit message.
    pub message: Option<String>,
    /// Changed paths (when requested).
    pub changed_paths: Vec<ChangedPath>,
    /// Entry has merged-in children.
    pub has_children: bool,
}

/// Local node status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeStatus {
    /// Not versioned status information.
    None,
    /// Unmodified.
    Normal,
    /// Scheduled for addition.
    Added,
    /// Scheduled for deletion.
    Deleted,
    /// Locally modified.
    Modified,
    /// Replaced.
    Replaced,
    /// Versioned but missing on disk.
    Missing,
    /// Wrong node kind on disk.
    Obstructed,
    /// Not versioned.
    Unversioned,
    /// Ignored.
    Ignored,
    /// Has a text conflict.
    Conflicted,
    /// Incomplete (interrupted update).
    Incomplete,
    /// Excluded by sparse depth.
    Excluded,
    /// Unversioned directory that holds an `svn:externals` working copy, or one of the
    /// unversioned directories leading to one (`svn status` prints `X`). Never removable as an
    /// unversioned item.
    External,
}

/// Property status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PropStatus {
    /// No properties.
    None,
    /// Unmodified.
    Normal,
    /// Modified.
    Modified,
    /// Conflicted.
    Conflicted,
}

/// Repository-side status for "check for modifications".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteStatus {
    /// A newer version exists in the repository.
    pub out_of_date: bool,
    /// Latest revision of the node in the repository.
    pub remote_rev: Option<Revnum>,
    /// Lock held in the repository.
    pub remote_lock: Option<LockInfo>,
}

/// Working-copy node state produced by the client layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WcStatus {
    /// Path relative to the working copy root.
    pub path: RelPath,
    /// Node kind.
    pub kind: NodeKind,
    /// Text/node status.
    pub text: NodeStatus,
    /// Property status.
    pub props: PropStatus,
    /// Node is a tree-conflict victim.
    pub tree_conflict: bool,
    /// Move source.
    pub moved_from: Option<RelPath>,
    /// Move target.
    pub moved_to: Option<RelPath>,
    /// Changelist name.
    pub changelist: Option<String>,
    /// Lock recorded in the working copy.
    pub lock: Option<LockInfo>,
    /// Lock token belongs to this working copy.
    pub lock_is_mine: bool,
    /// `svn:needs-lock` is set.
    pub needs_lock: bool,
    /// Content detected as binary.
    pub binary: bool,
    /// Size on disk.
    pub size: Option<u64>,
    /// Repository-side status, when checked.
    pub remote: Option<RemoteStatus>,
    /// Node is switched relative to its parent.
    pub switched: bool,
    /// Node is a file external (`svn status` prints `X` in the switched column).
    pub file_external: bool,
    /// Recorded depth.
    pub depth: Depth,
    /// Base revision.
    pub base_rev: Option<Revnum>,
}
