//! Crash-safety harness for the OpenSVN engine (phase 13).
//!
//! Real child processes ([`worker`], binary `crash-worker`) run checkout, update, commit, revert
//! and cleanup through `svn-ra` + `svn-client` + `svn-wc` against a local Apache `mod_dav_svn`
//! ([`server::Apache`]). The parent kills them with `SIGKILL`, either after a random delay or
//! at a named `fail` crate failpoint of `svn-wc`/`svn-client` (feature `failpoints`; the worker
//! sends itself `SIGKILL` when the failpoint is hit). Then [`verify`] checks the working copy with
//! the real `svn` client as oracle: database integrity, `svn status` agreeing with the engine,
//! no lost local modification, no silent revert of server changes, and recovery on the next open
//! followed by a successful update. Linux only (process liveness comes from `/proc`); on other
//! systems the crate is empty.
#![cfg(target_os = "linux")]

pub mod engine;
pub mod harness;
pub mod owner;
pub mod rng;
pub mod server;
pub mod svn;
pub mod tree;
pub mod verify;
pub mod worker;

/// Harness error: a message that says what failed and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<svn_types::SvnError> for Error {
    fn from(e: svn_types::SvnError) -> Self {
        Self(format!("engine: {e}"))
    }
}

/// Result of the harness.
pub type Result<T> = std::result::Result<T, Error>;

/// Builds an [`Error`].
pub fn err(msg: impl Into<String>) -> Error {
    Error(msg.into())
}
