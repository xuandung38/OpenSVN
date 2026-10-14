//! Working copy operations that need no network: add, delete, move, copy, revert, properties,
//! changelists, cleanup and conflict resolution.
//!
//! Every mutating function takes the working copy write lock (reusing one already held through
//! the same [`svn_wc::WcDb`] handle), finishes leftover work queue items first, and runs the
//! items it queued before returning, so a crash at any point is completed by the next
//! operation or by [`cleanup`]. The caller must set the lock owner on the handle
//! (`WcDb::set_lock_owner`, or open with `WcDb::open_and_recover`) before using these.

pub mod add;
pub mod autoprops;
pub mod changelist;
pub mod cleanup;
pub mod content;
pub mod delete;
pub mod ignore;
pub mod lock;
pub mod move_copy;
pub mod paths;
pub mod props;
pub mod resolve;
pub mod revert;
pub mod state;

pub use add::{AddOptions, AddOutcome, add, add_reporting};
pub use autoprops::AddConfig;
pub use changelist::set_changelist;
pub use cleanup::{CleanupReport, cleanup};
pub use delete::{DeleteOptions, delete};
pub use move_copy::{copy, mark_move, move_path};
pub use props::{PropSetOptions, ignore_add, propget, proplist, propset};
pub use resolve::{ResolveChoice, ResolveScope, resolve};
pub use revert::{RevertOptions, revert};
