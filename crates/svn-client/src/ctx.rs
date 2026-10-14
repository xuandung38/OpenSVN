//! Client context: configuration, lock ownership and progress reporting shared by operations.

use std::sync::Arc;

use svn_types::{ErrorCode, NodeKind, RelPath, Revnum};
use svn_wc::db::LockOwner;

use crate::externals::SessionOpener;

/// What happened to one path during an update, switch or checkout (the svn notification letters).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    /// `A`: added.
    Add,
    /// `D`: deleted.
    Delete,
    /// `U`: updated.
    Update,
    /// `G`: local changes merged cleanly with the incoming ones.
    Merged,
    /// `C`: text or property conflict.
    Conflicted,
    /// Tree conflict: the incoming change was not applied.
    TreeConflicted,
    /// `E`: an unversioned item that was in the way is now versioned.
    Existed,
    /// `R`: a versioned item that was missing on disk was restored.
    Restored,
    /// Skipped because an earlier conflict or obstruction covers the path.
    Skipped,
}

/// One notification emitted while an update drive is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateEvent {
    /// Working copy relative path.
    pub path: RelPath,
    /// Node kind.
    pub kind: NodeKind,
    /// What happened.
    pub action: UpdateAction,
}

/// Progress callback; called from the thread that applies the drive, so keep it cheap.
pub type ProgressFn = Arc<dyn Fn(&UpdateEvent) + Send + Sync>;

/// Client settings that influence how working files are written.
#[derive(Debug, Clone, Default)]
pub struct ClientConfig {
    /// Set a file's mtime to its last-changed date when it is written (`use-commit-times`).
    pub use_commit_times: bool,
    /// `preserved-conflict-file-exts` patterns appended to conflict marker names.
    pub preserved_conflict_exts: Vec<String>,
    /// `global-ignores` patterns; `None` selects svn's default list.
    pub global_ignores: Option<Vec<String>>,
    /// Most externals one checkout, update or switch processes, nested ones included; `None`
    /// selects [`crate::externals::MAX_EXTERNALS`].
    pub max_externals: Option<usize>,
}

/// Everything an operation needs besides the repository session and the cancel token.
#[derive(Clone)]
pub struct ClientCtx {
    /// Who records working copy locks taken by this process.
    pub lock_owner: LockOwner,
    /// Settings.
    pub config: ClientConfig,
    /// Optional progress sink.
    pub progress: Option<ProgressFn>,
    /// Fetches `svn:externals` after checkout, update and switch through sessions this opener
    /// hands out. `None` behaves like `svn --ignore-externals`: definitions are checked out like
    /// any other property and no external is fetched, updated or removed.
    pub externals: Option<Arc<dyn SessionOpener>>,
    /// How many externals deep the current operation runs (0 for the user's own operation).
    pub(crate) external_nesting: u32,
    /// `(repository UUID, repository path)` of every directory whose definition led to the
    /// current operation, outermost first (cycle detection).
    pub(crate) external_chain: Vec<(String, String)>,
    /// Externals processed so far by the user's operation, shared by every nested operation;
    /// `None` at the top level (a fresh count starts there).
    pub(crate) external_budget: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

impl std::fmt::Debug for ClientCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientCtx")
            .field("lock_owner", &self.lock_owner)
            .field("config", &self.config)
            .field("externals", &self.externals.is_some())
            .finish_non_exhaustive()
    }
}

impl ClientCtx {
    /// Creates a context with default configuration and no progress sink.
    pub fn new(lock_owner: LockOwner) -> Self {
        Self {
            lock_owner,
            config: ClientConfig::default(),
            progress: None,
            externals: None,
            external_nesting: 0,
            external_chain: Vec::new(),
            external_budget: None,
        }
    }

    pub(crate) fn notify(&self, event: &UpdateEvent) {
        if let Some(p) = &self.progress {
            p(event);
        }
    }
}

/// What happened to one external while its definitions were processed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalAction {
    /// Checked out for the first time.
    Fetched,
    /// An existing external was updated (or switched to a new URL of the same repository).
    Updated,
    /// The definition is gone and so is the external.
    Removed,
    /// The definition is gone but the external had local changes (or unversioned items) that
    /// were left on disk; see [`ExternalReport::message`].
    LeftLocalModifications,
    /// The external could not be processed; see [`ExternalReport::message`]. Other externals
    /// were still processed, like svn does (`svn: warning: W205011`).
    Failed,
}

/// The outcome for one external (svn's "Fetching external item into ..." notifications).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalReport {
    /// Where the external lives, relative to the working copy root of the operation (an
    /// external inside another external carries the full path).
    pub path: RelPath,
    /// File or directory external (`Unknown` when the kind could not be determined).
    pub kind: NodeKind,
    /// What happened.
    pub action: ExternalAction,
    /// Revision the external is at after the operation.
    pub revision: Option<Revnum>,
    /// Error code of a failure.
    pub code: Option<ErrorCode>,
    /// Warning or error text for the user.
    pub message: Option<String>,
}

impl ExternalReport {
    /// True when the user should be told about this external (a failure, or local changes
    /// that were kept).
    pub fn is_warning(&self) -> bool {
        matches!(
            self.action,
            ExternalAction::Failed | ExternalAction::LeftLocalModifications
        )
    }
}

/// Outcome of a checkout, update or switch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateSummary {
    /// Revision the working copy was brought to.
    pub revision: Option<svn_types::Revnum>,
    /// Every notification, in order (including those of externals, with full paths).
    pub events: Vec<UpdateEvent>,
    /// Externals processed after the main drive, in the order svn processes them; empty when
    /// externals are ignored ([`ClientCtx::externals`] is `None`).
    pub externals: Vec<ExternalReport>,
}

impl UpdateSummary {
    /// Externals that need the user's attention ([`ExternalReport::is_warning`]).
    pub fn external_warnings(&self) -> impl Iterator<Item = &ExternalReport> {
        self.externals.iter().filter(|e| e.is_warning())
    }

    /// Paths that ended in a text, property or tree conflict.
    pub fn conflicts(&self) -> Vec<&RelPath> {
        self.events
            .iter()
            .filter(|e| {
                matches!(
                    e.action,
                    UpdateAction::Conflicted | UpdateAction::TreeConflicted
                )
            })
            .map(|e| &e.path)
            .collect()
    }
}
