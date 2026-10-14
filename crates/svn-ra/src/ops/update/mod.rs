//! Update, switch and `status -u` over HTTP: the update-report drive.
//!
//! * [`RaWire`] is the per-session wiring (transport, capabilities, URL builder).
//! * [`run_update`] sends the working-copy report, parses the answer incrementally and drives a
//!   [`svn_types::DeltaEditor`] through an `EditorWorker`.

mod driver;
mod event_bridge;
mod fetch_scheduler;
mod wire;

pub use driver::run_update;
pub use wire::RaWire;
pub(crate) use wire::{buffered_error, buffered_headers, error_of, headers_of, read_limited};
