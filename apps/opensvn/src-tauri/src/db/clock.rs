//! Timestamp helper shared by the storage layer.

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Current time as an ISO-8601 UTC string (the wire and storage format).
pub fn now_iso() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}
