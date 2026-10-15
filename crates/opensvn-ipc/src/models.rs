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
    /// A checkout or update stopped before it finished (cancelled, connection lost): some folders
    /// are recorded as incomplete. Updating the working copy finishes it.
    pub incomplete: bool,
    /// How to finish an incomplete working copy at the revision it was reaching (`None` when it
    /// is complete).
    #[serde(default)]
    #[specta(optional)]
    pub resume: Option<ResumeInfo>,
}

/// An interrupted checkout or update, as the working copy records it: `wc_continue` finishes it
/// at `rev`, the revision it was reaching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ResumeInfo {
    pub rev: Rev,
    /// The working copy was never complete: a checkout was interrupted (otherwise an update).
    pub checkout: bool,
}

/// One row of the repository manager (`wc_entries`): every registered working copy, including
/// ones that can no longer be read (folder moved or deleted), which `wc_list` leaves out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct WcEntry {
    pub id: WcId,
    /// Display name (app database only; renaming never touches the folder).
    pub name: String,
    pub root_path: String,
    pub repo_root_url: String,
    pub repo_uuid: String,
    /// URL the working copy points at; `None` when it cannot be read.
    pub url: Option<String>,
    pub added_at: String,
    pub last_opened_at: Option<String>,
    /// The folder is gone, moved or no longer a working copy. The UI offers "Locate…" and "Remove".
    pub missing: bool,
    /// Why the working copy cannot be read (only when `missing`).
    pub problem: Option<String>,
    /// A checkout or update stopped before it finished; the UI offers to continue it. Always
    /// `false` when `missing`.
    pub incomplete: bool,
    /// See [`WorkingCopy::resume`].
    #[serde(default)]
    #[specta(optional)]
    pub resume: Option<ResumeInfo>,
}

/// What the running engine supports, so the UI can hide or explain options it would ignore
/// (`engine_capabilities`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct EngineCapabilities {
    /// Checkout, update and switch fetch `svn:externals`. When `false` they behave like
    /// `svn --ignore-externals` whatever `CheckoutRequest.ignore_externals` says
    /// (https://github.com/xuandung38/OpenSVN/issues/97).
    pub externals: bool,
    /// The in-memory demo engine runs in a release build: nothing is read from or written to a
    /// real repository, and the UI says so with a permanent banner. Always `false` in debug builds,
    /// where the demo engine is the development default.
    pub demo: bool,
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
    /// A folder an interrupted checkout or update did not finish (svn's `!` on a directory):
    /// not a local change and not an error; continuing the checkout or update finishes it.
    Incomplete,
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
    /// Always `None` over IPC: engines keep lock tokens to themselves (ownership is `mine`).
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
    /// Optional, only with `too_large`: an engine may send the changed hunks of a file too large
    /// to send in full, or nothing (the real engine does not read a side over 5 MB at all). The
    /// UI shows the large-file banner (load full / external tool) whenever `too_large` is set and
    /// there are no text sides, with or without hunks.
    pub hunks: Option<Vec<Hunk>>,
    pub prop_changes: Vec<PropChange>,
    pub image: Option<ImageSides>,
    /// Text larger than 5 MB: ask before loading (`force_large`). `left`/`right` are omitted;
    /// property rows are still sent.
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
    /// Working copy folder the operation writes to or reads (a checkout's target folder, which
    /// has no `wc_id` until it finishes). The UI matches it against a working copy's `root_path`
    /// to tell its own lock from a stale one.
    pub root_path: Option<String>,
    /// Started by the app itself (periodic update check), not by the user: the UI reports its
    /// failure quietly instead of as an error row.
    pub background: bool,
}

/// Coarse stage of a long operation, for the progress details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum OpStage {
    /// Opening the session (TLS, authentication).
    Connecting,
    /// Receiving the tree and file contents and writing them to the working copy.
    Fetching,
    /// The server drive is over; recording the result (externals, working copy info).
    Finishing,
    /// The connection was lost and the server cannot be reached: the operation probes it and
    /// resumes on its own once it answers ([`Progress::connection_wait`] has the timing).
    WaitingForConnection,
}

/// Timing of [`OpStage::WaitingForConnection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionWait {
    /// Seconds until the next attempt ("Retry now" tries at once).
    pub retry_in_secs: u32,
    /// Seconds since the connection was lost.
    pub waited_secs: u32,
    /// Seconds the operation waits in total before it stops; the working copy then stays
    /// incomplete and can be continued.
    pub max_wait_secs: u32,
    /// Attempts made so far.
    pub attempt: u32,
}

/// What happened to one path, as `svn` prints it in the first column (`A`, `U`, `D`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum PathAction {
    /// `A`
    Added,
    /// `D`
    Deleted,
    /// `U`
    Updated,
    /// `G`
    Merged,
    /// `C`
    Conflicted,
    /// `C` (tree conflict)
    TreeConflicted,
    /// `E`
    Existed,
    /// `R` (restored)
    Restored,
    /// Skipped
    Skipped,
}

/// One line of an operation's path log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEntry {
    pub action: PathAction,
    /// Working copy relative path (`""` for the root).
    pub path: String,
}

/// Paths handled so far per action (cumulative over the whole operation).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ActionCounts {
    pub added: u32,
    pub deleted: u32,
    pub updated: u32,
    pub merged: u32,
    pub conflicted: u32,
    pub tree_conflicted: u32,
    pub existed: u32,
    pub restored: u32,
    pub skipped: u32,
}

impl ActionCounts {
    /// Counts one more path under `action`.
    pub fn add(&mut self, action: PathAction) {
        let slot = match action {
            PathAction::Added => &mut self.added,
            PathAction::Deleted => &mut self.deleted,
            PathAction::Updated => &mut self.updated,
            PathAction::Merged => &mut self.merged,
            PathAction::Conflicted => &mut self.conflicted,
            PathAction::TreeConflicted => &mut self.tree_conflicted,
            PathAction::Existed => &mut self.existed,
            PathAction::Restored => &mut self.restored,
            PathAction::Skipped => &mut self.skipped,
        };
        *slot = slot.saturating_add(1);
    }
}

/// Progress sample of a running operation. Samples are coalesced by the backend (at most 20 per
/// second); `entries` carries the paths handled since the previous sample, newest last, of which
/// only the most recent are kept when there were many (`entriesDropped` counts the others).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub op_id: OpId,
    pub phase: String,
    pub current: Option<Size>,
    pub total: Option<Size>,
    /// Bytes received from the server so far (or written, for a download).
    pub bytes_done: Option<Size>,
    pub bytes_total: Option<Size>,
    pub current_path: Option<String>,
    pub stage: Option<OpStage>,
    pub entries: Vec<ProgressEntry>,
    pub entries_dropped: u32,
    pub counts: Option<ActionCounts>,
    /// Set while `stage` is [`OpStage::WaitingForConnection`].
    #[serde(default)]
    #[specta(optional)]
    pub connection_wait: Option<ConnectionWait>,
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

/// Result of `repo_resolve_url`: the URL to use, converted when the input was a VisualSVN
/// web-interface link.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedRepoUrl {
    /// Repository URL to use (the input unchanged when it was not a web link).
    pub url: String,
    /// Whether `url` was derived from a VisualSVN web-interface link and confirmed by the server.
    pub converted: bool,
    /// Revision the web link pointed at (`/view/r1234/...`), as a suggestion.
    pub peg_rev: Option<Rev>,
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
