//! IPC contract between the React UI and the Rust backend (docs/system-design/03).
//!
//! Plain data types only: `serde` for the wire format, `specta` for TypeScript generation.
//! The Tauri-specific wiring (commands, events) lives in `apps/opensvn/src-tauri`.

// The contract types mirror docs/system-design/03, which is their reference documentation.
#![allow(missing_docs)]

pub mod commands;
pub mod error;
pub mod events;
pub mod ids;
pub mod models;
pub mod path_grants;
pub mod settings;
pub mod status_map;
pub mod time_fmt;

pub use commands::*;
pub use error::{
    AppError, ErrorAction, ErrorCode, IncompleteCheckout, LockConflict, map_svn_error_code,
};
pub use events::*;
pub use ids::{OpId, Rev, Size, WcId};
pub use models::*;
pub use path_grants::*;
pub use settings::*;
pub use status_map::file_status_from_wc;
