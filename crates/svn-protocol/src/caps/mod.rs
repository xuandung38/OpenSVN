//! Session setup: the `OPTIONS` exchange, advertised capabilities and URL construction.

mod fallbacks;
mod options;
mod parent_listing;
mod url_builder;

pub use fallbacks::{BulkPreference, ListStrategy};
pub(crate) use options::advertise_caps;
pub use options::{BulkUpdates, Capabilities, DavCaps, options_request};
pub use parent_listing::{parent_listing_request, parent_path_error, parent_path_listing};
pub use url_builder::{AcceptedHref, HrefKind, TxnName, UrlBuilder, check_server_url_text};
