//! Work queue (track B): the `WORK_QUEUE` item codec ([`item`]) and the runner ([`runner`]).
//!
//! Items are skels `(op arg...)` written by svn in the same transaction as the metadata change
//! that needs them. A compatible client must finish whatever a previous svn/TortoiseSVN run
//! left in the queue before it writes anything; an item it does not know stops processing with
//! [`WorkQueueError::UnknownItem`] (the user must run `svn cleanup`), exactly like svn.
//! Builders and argument order were verified against `workqueue.c` at 1.14.5 and 1.15.0.

mod error;
pub mod item;
pub mod runner;

pub use error::WorkQueueError;
pub use item::WorkItem;
pub use runner::{
    FileInfoRecord, NodeInstallInfo, PristineSource, RunnerConfig, WorkQueueRunner, WorkQueueStore,
    WorkingCopyOps,
};

#[cfg(test)]
mod tests;
