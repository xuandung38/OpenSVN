//! Operation lifecycle: identifiers, events, progress coalescing, cancellation, history.

mod coalesce;
pub mod events;
mod manager;
mod throttle;

#[cfg(test)]
mod tests;

pub use events::{AppEvents, TauriEvents};
pub use manager::{OpSpec, OperationManager, new_op_id};
