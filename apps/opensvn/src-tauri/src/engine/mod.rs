//! Engine boundary: the [`EngineFacade`] trait, its per-call context and the fake engine.

#[cfg(any(test, feature = "engine-conformance"))]
pub mod conformance;
pub mod context;
pub mod facade;
pub mod fake;

pub use context::{OpContext, ProgressReporter, ProgressUpdate};
pub use facade::{
    DiffOptions, EngineFacade, EngineResult, LockScope, LogScope, RemoteScan, StatusQuery, WcInfo,
    WcRef,
};
pub use fake::FakeEngine;
