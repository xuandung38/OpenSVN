//! Working copy registry and the per-working-copy actor.

mod actor;
pub mod checkout_target;
mod registry;

#[cfg(test)]
mod tests;

#[cfg(test)]
pub use actor::WcActor;
pub use registry::{ResolvedWc, WcRegistry};
