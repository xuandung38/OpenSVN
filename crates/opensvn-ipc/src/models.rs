//! Data model from docs/system-design/03 section 2.
//!
//! Wire format is camelCase JSON; string unions become Rust enums with matching renames.
//! Timestamps are ISO-8601 UTC strings; paths inside a working copy are `/`-separated relpaths.

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::ids::{OpId, Rev, Size, WcId};
use crate::path_grants::PathGrantId;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct BranchRef {
    pub kind: BranchKind,
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum BranchKind {
    Trunk,
    Branch,
    Tag,
    Other,
}

/// Working-copy format; only 31 and 32 are valid (validated on deserialize; `number` in TS).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(try_from = "u8", into = "u8")]
#[specta(type = u8)]
pub enum WcFormat {
    V31,
    V32,
}

impl From<WcFormat> for u8 {
    fn from(f: WcFormat) -> u8 {
        match f {
            WcFormat::V31 => 31,
            WcFormat::V32 => 32,
        }
    }
}

impl TryFrom<u8> for WcFormat {
    type Error = String;
    fn try_from(v: u8) -> Result<Self, String> {
        match v {
            31 => Ok(WcFormat::V31),
            32 => Ok(WcFormat::V32),
            other => Err(format!("unsupported working copy format {other}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct WorkingCopy {
    pub id: WcId,
    pub name: String,
    pub root_path: String,
    pub repo_root_url: String,
    pub repo_uuid: String,
    pub url: String,
    pub base_rev: Rev,
    pub rev_range: Option<(Rev, Rev)>,
    pub format: WcFormat,
    pub store_pristine: bool,
    /// `None` when the repo has no trunk/branches/tags layout (UI falls back to Location mode).
    pub branch: Option<BranchRef>,
    pub path_in_repo: String,
    /// Number of new revisions not yet updated.
    pub incoming: u32,
    pub last_checked_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum NodeStatus {
    Normal,
    Modified,
    Added,
    Deleted,
    Replaced,
    Unversioned,
    Missing,
    Conflicted,
    Ignored,
    Obstructed,
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum PropStatus {
    None,
    Normal,
    Modified,
    Conflicted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct LockInfo {
    pub owner: String,
    pub comment: Option<String>,
    pub created_at: String,
    pub token: Option<String>,
    pub mine: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RemoteStatus {
    pub out_of_date: bool,
    pub remote_rev: Option<Rev>,
    pub remote_lock: Option<LockInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FileStatus {
    pub path: String,
    pub kind: FileKind,
    pub text: NodeStatus,
    pub props: PropStatus,
    pub tree_conflict: bool,
    pub moved_from: Option<String>,
    pub moved_to: Option<String>,
    pub changelist: Option<String>,
    pub lock: Option<LockInfo>,
    pub needs_lock: bool,
    pub remote: Option<RemoteStatus>,
    pub binary: bool,
    pub size: Option<Size>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub rev: Rev,
    pub author: Option<String>,
    pub date: String,
    pub message: String,
    pub changed_paths: Option<Vec<ChangedPath>>,
    /// Has merged child revisions.
    pub has_children: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum ChangeAction {
    A,
    M,
    D,
    R,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum PathKind {
    File,
    Dir,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CopyFrom {
    pub path: String,
    pub rev: Rev,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ChangedPath {
    pub path: String,
    pub action: ChangeAction,
    pub kind: PathKind,
    pub copy_from: Option<CopyFrom>,
    pub text_mods: Option<bool>,
    pub prop_mods: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    File,
    Dir,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RepoEntry {
    pub name: String,
    pub kind: EntryKind,
    pub size: Option<Size>,
    pub created_rev: Rev,
    pub last_author: Option<String>,
    pub date: String,
    pub lock: Option<LockInfo>,
}

/// What to diff. Tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DiffTarget {
    /// base <-> working
    Wc {
        wc_id: WcId,
        path: String,
    },
    /// rev-1 <-> rev
    Rev {
        url: String,
        rev: Rev,
    },
    Range {
        url: String,
        from: Rev,
        to: Rev,
    },
    WcVsRev {
        wc_id: WcId,
        path: String,
        rev: Rev,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum Eol {
    #[serde(rename = "LF")]
    Lf,
    #[serde(rename = "CRLF")]
    Crlf,
    #[serde(rename = "mixed")]
    Mixed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TextSide {
    pub content: String,
    pub encoding: String,
    pub eol: Eol,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct Hunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PropChange {
    pub name: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ImageSides {
    pub left_url: Option<String>,
    pub right_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FileDiff {
    pub path: String,
    pub binary: bool,
    pub mime: Option<String>,
    pub left: Option<TextSide>,
    pub right: Option<TextSide>,
    /// Only hunks are sent when the file is too large.
    pub hunks: Option<Vec<Hunk>>,
    pub prop_changes: Vec<PropChange>,
    pub image: Option<ImageSides>,
    /// Text larger than 5 MB: ask before loading.
    pub too_large: Option<bool>,
    /// The two sides differ too much to diff within the display budget
    /// (`svn_diff::DISPLAY_MAX_COST`). `left`/`right` are then omitted and the UI offers the
    /// external diff tool (`diff_external`) instead; it never retries the display diff.
    pub too_complex: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum ConflictKind {
    Text,
    Property,
    Tree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum TreeReason {
    Edited,
    Deleted,
    Missing,
    Moved,
    Added,
    Replaced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum TreeAction {
    Edit,
    Delete,
    Add,
    Replace,
    Move,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct Conflict {
    pub path: String,
    pub kind: ConflictKind,
    /// Temp file paths for the three sides.
    pub base: Option<String>,
    pub mine: Option<String>,
    pub theirs: Option<String>,
    pub tree_reason: Option<TreeReason>,
    pub tree_action: Option<TreeAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum OperationKind {
    Checkout,
    Update,
    Switch,
    Commit,
    Revert,
    Cleanup,
    Lock,
    Unlock,
    StatusRemote,
    Log,
    Blame,
    RemoteEdit,
    SetDepth,
    Upgrade,
    Merge,
    /// Local status rescan (`status_refresh`).
    StatusRefresh,
    Add,
    Delete,
    Move,
    /// Server-side copy (`copy_location`).
    Copy,
    /// Download of a repository file to disk (`repo_save`).
    Save,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct OperationHandle {
    pub op_id: OpId,
    pub kind: OperationKind,
    pub wc_id: Option<WcId>,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub op_id: OpId,
    pub phase: String,
    pub current: Option<Size>,
    pub total: Option<Size>,
    pub bytes_done: Option<Size>,
    pub bytes_total: Option<Size>,
    pub current_path: Option<String>,
}

/// SVN has no real branches/tags: layout is a naming convention that may not exist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum Layout {
    /// Auto-detect trunk/branches/tags.
    Auto,
    /// Patterns, e.g. `/{project}/trunk`.
    Custom {
        trunk: String,
        branches: Vec<String>,
        tags: Vec<String>,
    },
    /// Folder-organised repo, no branches.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum Depth {
    Empty,
    Files,
    Immediates,
    Infinity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum SparseDepth {
    Empty,
    Files,
    Immediates,
    Infinity,
    Exclude,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SparseEntry {
    pub path: String,
    pub depth: SparseDepth,
}

/// Node of the branch tree returned by `branches_list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct BranchNode {
    pub kind: BranchKind,
    pub name: String,
    pub url: String,
}

/// Result of `repo_info`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RepoInfo {
    pub repo_root_url: String,
    pub repo_uuid: String,
    pub youngest_rev: Rev,
    pub kind: EntryKind,
}

/// Result of `layout_detect`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct LayoutDetection {
    pub found: bool,
    pub layout: Option<Layout>,
    pub current: Option<BranchRef>,
}

/// Result of `log_get`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct LogPage {
    pub entries: Vec<LogEntry>,
    pub next_rev: Option<Rev>,
}

/// Where a log query reads from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum LogTarget {
    Url { url: String },
    Wc { wc_id: WcId, path: String },
}

/// Which items a lock/unlock applies to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum LockTargets {
    Wc { wc_id: WcId, paths: Vec<String> },
    Url { urls: Vec<String> },
}

/// One operation of `remote_edit`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RemoteOp {
    Mkdir {
        url: String,
    },
    Rename {
        from_url: String,
        to_url: String,
    },
    Delete {
        url: String,
    },
    Copy {
        from_url: String,
        from_rev: Option<Rev>,
        to_url: String,
    },
    Upload {
        url: String,
        /// File chosen with `path_pick`.
        source: PathGrantId,
    },
}

/// Who may see an `ignore_add` rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum IgnoreScope {
    #[serde(rename = "svn:ignore")]
    SvnIgnore,
    #[serde(rename = "local")]
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum ResolveChoice {
    Mine,
    Theirs,
    Working,
    Base,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum OpenWith {
    Default,
    Editor,
    Terminal,
}

/// Blame result line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct BlameLine {
    pub rev: Rev,
    pub author: Option<String>,
    pub date: String,
    pub text: String,
}

/// A certificate the user chose to trust permanently (Settings > Certificates).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TrustedCert {
    pub host: String,
    pub port: u16,
    pub sha256: String,
    pub subject: Option<String>,
    pub expires_at: Option<String>,
    pub trusted_at: String,
}

/// Identifies a trusted certificate for removal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TrustedCertKey {
    pub host: String,
    pub port: u16,
    pub sha256: String,
}
