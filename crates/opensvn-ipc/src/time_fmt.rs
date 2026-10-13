//! Timestamp conversion: engine timestamps are `i64` microseconds since the Unix epoch,
//! the IPC wire format is an ISO-8601 UTC string.

use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// Converts microseconds since the Unix epoch to an RFC 3339 UTC string.
/// Returns `None` when the value is outside the representable range.
pub fn micros_to_iso(micros: i64) -> Option<String> {
    let nanos = i128::from(micros).checked_mul(1000)?;
    OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .ok()?
        .format(&Rfc3339)
        .ok()
}

/// Engine timestamp -> ISO-8601 UTC string; out-of-range values fall back to the Unix epoch.
pub fn timestamp_to_iso(ts: svn_types::Timestamp) -> String {
    micros_to_iso(ts.0).unwrap_or_else(|| "1970-01-01T00:00:00Z".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_and_fractional() {
        assert_eq!(micros_to_iso(0).as_deref(), Some("1970-01-01T00:00:00Z"));
        assert_eq!(
            micros_to_iso(1_760_000_000_123_456).as_deref(),
            Some("2025-10-09T08:53:20.123456Z")
        );
    }

    #[test]
    fn timestamp_wrapper_and_fallback() {
        assert_eq!(
            timestamp_to_iso(svn_types::Timestamp(1_000_000)),
            "1970-01-01T00:00:01Z"
        );
        assert_eq!(
            timestamp_to_iso(svn_types::Timestamp(i64::MAX)),
            "1970-01-01T00:00:00Z"
        );
    }

    #[test]
    fn out_of_range_is_none() {
        assert_eq!(micros_to_iso(i64::MAX), None);
    }
}
