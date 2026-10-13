//! Command handlers for every command in docs/system-design/03 section 3.
//!
//! Handlers are thin: they resolve state, call a service and return `AppError` on failure. Every
//! body runs inside [`crate::boundary::guard`] so a panic never takes the app down. Commands that
//! belong to a later service (auth prompts, layout detection, OS integration, diagnostics) are
//! still `not implemented` stubs.

mod remote;
mod system;
mod working_copy;
mod write_ops;

pub use remote::*;
pub use system::*;
pub use working_copy::*;
pub use write_ops::*;
