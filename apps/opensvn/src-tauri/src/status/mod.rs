//! Status publication: compares fresh status with the cached snapshot and emits only the delta.

mod publisher;

pub use publisher::StatusPublisher;
