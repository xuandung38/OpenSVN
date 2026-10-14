//! Update, switch and checkout: the editor that applies a server drive to a working copy and the
//! driver that surrounds it with authentication, locking and recovery.
//!
//! # Atomicity
//! Per directory and per file, the new BASE row, the local property overlay, the conflict record
//! and the work items that make the disk match are written in ONE wc.db transaction, after the
//! new pristine text and any merge result were fully written (and synced) to `.svn/tmp`. A crash
//! before the transaction leaves the previous state; a crash after it leaves the new state plus
//! queued work that the next operation completes. The state "new base, working file derived from
//! the old base" cannot occur, so an interrupted update never silently reverts server changes.
//!
//! # Order of operations
//! 1. [`svn_ra_api::RaSession::ensure_authenticated`] (may prompt) - no lock is held yet.
//! 2. Take the working copy lock, run leftover work items.
//! 3. Report, drive the editor, run queued work after each node.
//! 4. Release the lock (also on error and cancellation).

mod conflict_record;
mod editor;
mod file_install;
mod finish;
mod props;
mod reporter;
mod restore;
mod run;
mod state;
mod text_sink;

pub use editor::UpdateEditor;
pub use props::{PropMerge, merge_props};
pub use reporter::build_report;
pub use run::{UpdateKind, UpdateParams, anchor_of, drive_update, repos_relpath_of, root_string};
pub(crate) use state::tmp_dir as tmp_dir_of;
pub use state::{Outcome, OutcomeHandle, UpdateSetup, format_svn_date, parse_svn_date};
