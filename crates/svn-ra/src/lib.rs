//! Subversion RA layer over HTTP (WebDAV). Module layout is fixed; each module is owned by a separate work package.

pub mod auth;
pub mod ops;
pub mod session;
pub mod transport;
