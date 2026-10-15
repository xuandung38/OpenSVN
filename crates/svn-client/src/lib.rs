//! High-level working copy operations built on `svn-wc`, `svn-diff` and the RA contract.
//!
//! Track U: [`checkout`], [`update`], [`switch`] (module [`update`] and [`ops`]), local and remote
//! [`status`] and [`diff`]. Every operation that writes authenticates the session BEFORE it takes
//! the working copy lock, applies server changes through [`update::UpdateEditor`] (one database
//! transaction per file or directory, together with the work items that make the disk match), and
//! honours the caller's [`svn_types::CancelToken`].

pub mod checkout;
pub mod commit;
pub mod ctx;
pub mod diff;
pub mod externals;
pub mod local;
pub mod netwait;
pub mod ops;
pub mod sparse;
pub mod status;
pub mod update;
pub mod wc;

pub use checkout::checkout;
pub use ctx::{
    ClientConfig, ClientCtx, ExternalAction, ExternalReport, ProgressFn, UpdateAction, UpdateEvent,
    UpdateSummary,
};
pub use diff::{DiffOptions, diff};
pub use netwait::{NetworkWait, WaitState};
pub use ops::{switch, update};
pub use sparse::set_depth;
pub use status::{StatusOptions, status, status_remote};
