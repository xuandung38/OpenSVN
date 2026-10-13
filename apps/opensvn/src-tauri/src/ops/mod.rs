//! Operation lifecycle: identifiers, events, progress throttling, cancellation, history.

pub mod events;
mod manager;
mod throttle;

#[cfg(test)]
mod tests;

pub use events::{AppEvents, TauriEvents};
pub use manager::{OperationManager, new_op_id};
