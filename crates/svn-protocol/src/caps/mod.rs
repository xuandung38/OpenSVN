//! Session setup: the `OPTIONS` exchange, advertised capabilities and URL construction.

mod options;
mod url_builder;

pub(crate) use options::advertise_caps;
pub use options::{BulkUpdates, Capabilities, DavCaps, options_request};
pub use url_builder::{AcceptedHref, HrefKind, UrlBuilder};
