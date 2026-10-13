//! Path grants (see `opensvn_ipc::path_grants`): native dialogs and native drag and drop mint
//! opaque, single-use, time-limited ids; commands redeem them. The WebView never supplies a raw
//! path outside a registered working copy.

mod native;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use opensvn_ipc::{AppError, GrantKind, PathGrantId, PathGrantInfo, PickRequest};

use crate::errors::invalid_argument;

pub use native::NativePathPicker;

/// How long a grant stays redeemable.
pub const GRANT_TTL: Duration = Duration::from_secs(5 * 60);

/// The only window allowed to redeem grants.
pub const MAIN_WINDOW: &str = "main";

struct Grant {
    path: PathBuf,
    kind: GrantKind,
    window: String,
    minted: Instant,
}

/// Registry of outstanding grants.
pub struct PathGrants {
    ttl: Duration,
    grants: Mutex<HashMap<String, Grant>>,
}

impl PathGrants {
    /// Registry with the standard time to live.
    pub fn new() -> Self {
        Self::with_ttl(GRANT_TTL)
    }

    /// Registry with a custom time to live (tests).
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            ttl,
            grants: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Grant>> {
        self.grants.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Mints a grant for `path`, bound to `window`. Expired grants are dropped on the way.
    pub fn mint(&self, path: PathBuf, kind: GrantKind, window: &str) -> PathGrantInfo {
        let id = crate::ops::new_op_id().0; // UUID v7: unguessable enough together with single use
        let display_name = path.file_name().map_or_else(
            || path.to_string_lossy().into_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
        let mut grants = self.lock();
        let ttl = self.ttl;
        grants.retain(|_, g| g.minted.elapsed() < ttl);
        grants.insert(
            id.clone(),
            Grant {
                path,
                kind,
                window: window.to_owned(),
                minted: Instant::now(),
            },
        );
        PathGrantInfo {
            id: PathGrantId(id),
            display_name,
            kind,
        }
    }

    /// Redeems a grant for the given window. The grant is consumed even when the kind does not
    /// match, so a wrong guess cannot be retried.
    pub fn redeem(
        &self,
        id: &PathGrantId,
        window: &str,
        expected: GrantKind,
    ) -> Result<PathBuf, AppError> {
        let grant = self.lock().remove(&id.0);
        let Some(g) = grant else {
            return Err(invalid_argument(
                "The selected location is no longer valid. Choose it again.",
            ));
        };
        if g.minted.elapsed() >= self.ttl {
            return Err(invalid_argument(
                "The selected location expired. Choose it again.",
            ));
        }
        if g.window != window {
            return Err(invalid_argument(
                "The selected location belongs to another window.",
            ));
        }
        if g.kind != expected {
            return Err(invalid_argument(match expected {
                GrantKind::Dir => "Select a folder.",
                GrantKind::File => "Select a file.",
            }));
        }
        Ok(g.path)
    }

    /// Outstanding grants (tests).
    #[cfg(test)]
    pub fn outstanding(&self) -> usize {
        self.lock().len()
    }
}

impl Default for PathGrants {
    fn default() -> Self {
        Self::new()
    }
}

/// Native dialogs that return a path.
#[async_trait]
pub trait PathPicker: Send + Sync {
    /// Shows the dialog for `request`; `None` when the user cancels.
    async fn pick(&self, request: &PickRequest) -> Option<(PathBuf, GrantKind)>;
}

#[cfg(test)]
mod tests;
