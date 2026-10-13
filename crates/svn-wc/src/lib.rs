//! Subversion working copy engine.
//!
//! Reads and writes the `.svn/` administrative area in the libsvn_wc on-disk format
//! (format 31 for read/write, format 32 read-only). Module ownership is split between two
//! tracks inside this crate:
//!
//! * track A: [`skel`] and [`db`] (schema, node layers, wc locks, misc tables) plus this facade;
//! * track B: [`pristine`], [`translate`], [`workqueue`], [`conflicts`], [`status`].
//!
//! Track B modules reach the database only through [`db::WcDb`]; new facade functions are
//! added through small pull requests against `src/db/mod.rs`.

pub mod conflicts;
pub mod db;
pub mod pristine;
pub mod skel;
pub mod status;
pub mod translate;
pub mod workqueue;

pub use db::WcDb;
