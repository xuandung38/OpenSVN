//! Display-only repository paths and the records that carry them.
//!
//! Log and lock listings must show whatever the repository contains (`/trunk/aux.c`, `/a:b`), so
//! these paths get structural validation only. To keep them away from the file system the type
//! has a private field, no `Deref`, no `AsRef<Path>` and no conversion to `RelPath` or `Path`.
//! Code that needs a file system path must say so at the call site:
//! `RelPath::from_server(display.as_str().trim_start_matches('/'))`.

use std::fmt;

use svn_types::{
    ChangeAction, ChangedPath, LockInfo, NodeKind, RelPath, Result, Revnum, Timestamp,
};

use crate::util::validate_display_path;

/// Repository-absolute path (`/trunk/a.txt`) for display. Not usable for I/O.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoDisplayPath(String);

impl RepoDisplayPath {
    /// Validates structure only: absolute, no `.`/`..`/empty segment, no NUL.
    pub fn parse(path: &str) -> Result<Self> {
        validate_display_path(path).map(Self)
    }

    /// Text for display. Never pass it to a file system API; see the module documentation.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn from_rel(path: &RelPath) -> Self {
        Self(format!("/{path}"))
    }
}

impl fmt::Display for RepoDisplayPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One changed path of a log entry, display-only counterpart of `svn_types::ChangedPath`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayChangedPath {
    /// Path.
    pub path: RepoDisplayPath,
    /// What happened.
    pub action: ChangeAction,
    /// Node kind.
    pub kind: NodeKind,
    /// Copy source path and revision.
    pub copy_from: Option<(RepoDisplayPath, Revnum)>,
    /// Whether file text changed.
    pub text_mods: bool,
    /// Whether properties changed.
    pub prop_mods: bool,
}

impl DisplayChangedPath {
    /// Leaves the display-only type for the shared contract (`path` becomes a plain `String`).
    /// Call this only at the boundary where the contract demands it.
    pub fn into_contract(self) -> ChangedPath {
        ChangedPath {
            path: self.path.0,
            action: self.action,
            kind: self.kind,
            copy_from: self.copy_from.map(|(p, r)| (p.0, r)),
            text_mods: self.text_mods,
            prop_mods: self.prop_mods,
        }
    }
}

/// A repository lock whose path is display-only; counterpart of `svn_types::LockInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoLock {
    /// Locked path.
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

impl RepoLock {
    /// Leaves the display-only type for the shared contract (`path` becomes a plain `String`).
    pub fn into_lock_info(self) -> LockInfo {
        LockInfo {
            path: self.path.0,
            token: self.token,
            owner: self.owner,
            comment: self.comment,
            created: self.created,
            expires: self.expires,
        }
    }
}
