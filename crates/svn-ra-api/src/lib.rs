//! Async contracts between the client layer and the Subversion RA implementation.
//!
//! * [`RaSession`] is the repository session. Updates drive a synchronous
//!   [`svn_types::DeltaEditor`] through [`EditorWorker`] (blocking thread plus a bounded
//!   command channel); commits are driven by the caller through [`CommitEditor`].
//! * [`AuthPrompter`] lets the transport ask the app for credentials and certificate trust.
//! * With the `fake` feature the crate also ships an in-memory repository and test editors.

mod auth;
mod commit;
mod editor_worker;
mod session;

#[cfg(feature = "fake")]
pub mod fake;

pub use auth::{
    AuthMethod, AuthPrompter, CertDecision, CertFailure, CertInfo, CredentialRequest, Credentials,
    PromptReason,
};
pub use commit::{
    CommitEditor, CommitInfo, CommitObserver, NOTHING_COMMITTED, ReplayableSource,
    unsent_commit_error,
};
pub use editor_worker::{DEFAULT_COMMAND_CAPACITY, EditorHandle, EditorWorker, WindowSender};
pub use session::{
    DirListing, FileMeta, FileRev, FileRevHandler, InheritedItem, LocationSegment, LogRequest,
    RaSession, ReportFn, TxnAbort, UpdateMode, UpdateRequest,
};
