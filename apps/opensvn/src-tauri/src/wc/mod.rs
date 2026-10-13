//! Working copy registry and the per-working-copy actor.

mod actor;
mod registry;

#[cfg(test)]
mod tests;

pub use registry::{ResolvedWc, WcRegistry};
