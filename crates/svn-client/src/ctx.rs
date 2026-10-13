//! Client context: configuration, lock ownership and progress reporting shared by operations.

use std::sync::Arc;

use svn_types::{NodeKind, RelPath};
use svn_wc::db::LockOwner;

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
}

impl std::fmt::Debug for ClientCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientCtx")
            .field("lock_owner", &self.lock_owner)
            .field("config", &self.config)
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
        }
    }

    pub(crate) fn notify(&self, event: &UpdateEvent) {
        if let Some(p) = &self.progress {
            p(event);
        }
    }
}

/// Outcome of a checkout, update or switch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateSummary {
    /// Revision the working copy was brought to.
    pub revision: Option<svn_types::Revnum>,
    /// Every notification, in order.
    pub events: Vec<UpdateEvent>,
}

impl UpdateSummary {
    /// Paths that ended in a text, property or tree conflict.
    pub fn conflicts(&self) -> Vec<&RelPath> {
        self.events
            .iter()
            .filter(|e| matches!(e.action, UpdateAction::Conflicted | UpdateAction::TreeConflicted))
            .map(|e| &e.path)
            .collect()
    }
}
