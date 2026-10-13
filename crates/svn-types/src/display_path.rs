//! Display-only repository paths.
//!
//! Log and lock listings must show whatever the repository contains (`/trunk/aux.c`, `/a:b`),
//! so these paths get structural validation only. To keep them away from the file system the
//! type has a private field and deliberately offers no `Deref`, no `AsRef<Path>` and no
//! conversion to `RelPath` or `Path`. Code that needs a file system path must say so at the call
//! site: `RelPath::from_server(display.as_str().trim_start_matches('/'))`.

use std::fmt;

use crate::error::{ErrorCode, Result, SvnError};
use crate::rel_path::RelPath;

/// Repository-absolute path (`/trunk/a.txt`) for display. Not usable for I/O.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoDisplayPath(String);

impl RepoDisplayPath {
    /// Validates structure only: absolute, no `.`/`..`/empty segment, no control characters.
    /// The root is `/`.
    pub fn parse(path: &str) -> Result<Self> {
        let rel = path.strip_prefix('/').ok_or_else(|| {
            SvnError::new(
                ErrorCode::Corrupt,
                format!("repository path is not absolute: {path:?}"),
            )
        })?;
        let bad = !rel.is_empty()
            && (rel.chars().any(char::is_control)
                || rel.split('/').any(|s| matches!(s, "" | "." | "..")));
        if bad {
            return Err(SvnError::new(
                ErrorCode::UnsafePath,
                format!("unsafe repository path from server: {path:?}"),
            ));
        }
        Ok(Self(path.to_owned()))
    }

    /// Display form of an already validated relative path (`a/b` becomes `/a/b`).
    pub fn from_rel(path: &RelPath) -> Self {
        Self(format!("/{path}"))
    }

    /// Text for display. Never pass it to a file system API; see the module documentation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RepoDisplayPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for RepoDisplayPath {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}
