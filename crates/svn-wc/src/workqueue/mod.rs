//! Work queue (track B): the `WORK_QUEUE` item codec ([`item`]) and the runner ([`runner`]).
//!
//! Items are skels `(op arg...)` written by svn in the same transaction as the metadata change
//! that needs them. A compatible client must finish whatever a previous svn/TortoiseSVN run
//! left in the queue before it writes anything; an item it does not know stops processing with
//! [`WorkQueueError::UnknownItem`] (the user must run `svn cleanup`), exactly like svn.
//! Builders and argument order were verified against `workqueue.c` at 1.14.5 and 1.15.0.
//!
//! # Crash idempotency contract
//! Every item may run more than once (a crash can happen after the effect but before the item is
//! marked done; svn tests this with `SVN_RUN_WORK_QUEUE_TWICE`). The filesystem items in
//! [`runner`] are idempotent, and every [`WorkingCopyOps`] method must be too.
//!
//! # Intentional divergences from svn
//! * The legacy `tmp-set-text-conflict-markers` and `tmp-set-property-conflict-marker` items are
//!   rejected as unknown. svn 1.15 still dispatches them, but they are never persisted by
//!   current clients and `svn upgrade` refuses a non-empty queue, so a format-31/32 working copy
//!   cannot contain them; if one does, the error asks for `svn cleanup` with the svn that
//!   created the working copy.

mod error;
pub mod item;
pub(crate) mod paths;
pub mod runner;

pub use error::WorkQueueError;
pub use item::WorkItem;
pub use runner::{
    FileInfoRecord, NodeInstallInfo, PristineSource, RunnerConfig, WorkQueueRunner, WorkQueueStore,
    WorkingCopyOps,
};

#[cfg(test)]
mod tests;
