//! IPC contract between the React UI and the Rust backend (docs/system-design/03).
//!
//! Plain data types only: `serde` for the wire format, `specta` for TypeScript generation.
//! The Tauri-specific wiring (commands, events) lives in `apps/opensvn/src-tauri`.

pub mod commands;
pub mod error;
pub mod events;
pub mod ids;
pub mod models;
pub mod settings;
pub mod time_fmt;

pub use commands::*;
pub use error::{
    ALL_SVN_ERROR_CODES, AppError, ErrorAction, ErrorCode, SvnErrorCode, map_svn_error_code,
};
pub use events::*;
pub use ids::{OpId, Rev, Size, WcId};
pub use models::*;
pub use settings::*;
