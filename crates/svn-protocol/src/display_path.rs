//! Display-only repository paths.
//!
//! The type itself lives in `svn-types` (`ChangedPath`, `LockInfo` and the copy sources use it);
//! it is re-exported here so existing imports keep working. Log and lock listings must show
//! whatever the repository contains (`/trunk/aux.c`, `/a:b`), so the paths get structural
//! validation only and cannot be turned into file system paths without the explicit
//! `RelPath::from_server(display.as_str().trim_start_matches('/'))` at the call site.

pub use svn_types::RepoDisplayPath;
