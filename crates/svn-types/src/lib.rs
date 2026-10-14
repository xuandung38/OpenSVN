//! Shared engine contracts: plain data types, error model, path safety and the
//! synchronous delta-editor traits. This crate must not depend on an async runtime.

mod admin_name;
mod cancel;
mod display_path;
mod editor;
mod error;
mod ids;
mod lock_ownership;
mod model;
mod rel_path;
mod safe_join;

pub use admin_name::is_any_admin_dir_name;
pub use cancel::{CancelToken, Cancelled};
pub use display_path::RepoDisplayPath;
pub use editor::{DeltaEditor, Reporter, TxDeltaOp, TxDeltaWindow, WindowSink};
pub use error::{ConnectionFailure, ErrorCode, Result, SvnError};
pub use ids::{DirId, FileId, OriginKey, RepoUrl, Revnum, Timestamp};
pub use lock_ownership::{AliveProbe, LockOwnershipStore, MemoryLockOwnership};
pub use model::{
    ChangeAction, ChangedPath, Checksum, Depth, DirEntry, LockInfo, LogEntry, NodeKind, NodeStatus,
    PropChange, PropStatus, RemoteStatus, WcStatus,
};
pub use rel_path::RelPath;
pub use safe_join::{rename_replacing, safe_join, safe_join_child, write_file_replacing};
