//! Status publication: compares fresh status with the cached snapshot and emits only the delta.

mod debounce;
mod poller;
mod publisher;
mod remote_check;
mod service;
mod watcher;

pub use poller::{IncomingPoller, PollerDeps};
pub use publisher::StatusPublisher;
pub use remote_check::run_remote_check;
pub use service::{StatusService, StatusTiming};
