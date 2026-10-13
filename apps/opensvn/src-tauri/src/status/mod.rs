//! Status publication: compares fresh status with the cached snapshot and emits only the delta.

mod debounce;
mod publisher;
mod service;
mod watcher;

pub use publisher::StatusPublisher;
pub use service::{StatusService, StatusTiming};
