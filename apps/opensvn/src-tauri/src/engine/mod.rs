//! Engine boundary: the [`EngineFacade`] trait, its per-call context and the fake engine.

#[cfg(any(test, feature = "engine-conformance"))]
pub mod conformance;
pub mod context;
pub mod facade;
pub mod fake;
pub(crate) mod mapping;
#[cfg(feature = "real-engine")]
pub mod real;
pub(crate) mod text;

pub use context::{OpContext, ProgressReporter, ProgressUpdate};
pub use facade::{
    CheckoutPlan, DiffOptions, EditOp, EngineFacade, EngineResult, ExportedSides, LockScope,
    LogScope, RecoveryReport, RemoteEditPlan, RemoteScan, SideFiles, StatusQuery, WcInfo, WcOpened,
    WcRef,
};
pub use fake::FakeEngine;
