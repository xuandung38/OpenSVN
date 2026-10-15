//! Remote operations built on the session. Submodule files are owned per work package.
//!
//! [`HttpRaSession`] (in `open`) implements [`svn_ra_api::RaSession`] on top of the shared
//! [`update::RaWire`]: read operations live in `read`, `log`, `locks` and `file_revs`, the
//! update drive in `update` and the commit editor in `commit`.

pub mod blame;
pub mod commit;
pub mod file_revs;
pub mod locks;
pub mod log;
pub mod open;
pub mod read;
pub mod remote_edit;
pub(crate) mod resume;
pub mod update;

pub use open::HttpRaSession;
