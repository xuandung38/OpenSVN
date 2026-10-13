//! Small helpers shared by the protocol modules.

use svn_types::{ErrorCode, Revnum, SvnError, Timestamp};

/// Error for data that violates the wire protocol.
pub(crate) fn corrupt(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::Corrupt, msg)
}

/// Decodes `%XX` escapes. Invalid escapes and non UTF-8 results are rejected.
pub(crate) fn percent_decode(s: &str) -> Result<String, SvnError> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| corrupt(format!("bad percent escape in {s:?}")))?;
            out.push(hex);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| corrupt(format!("path is not UTF-8 after decoding: {s:?}")))
}

/// Parses a revision number as sent by the server.
pub(crate) fn parse_revnum(s: &str) -> Result<Revnum, SvnError> {
    s.trim()
        .parse::<u64>()
        .map(Revnum)
        .map_err(|_| corrupt(format!("invalid revision number {s:?}")))
}

/// Validates a repository-absolute path (`/trunk/a.txt`) received from the server and returns it
/// unchanged. Used only for display-only values: structure rules only (absolute, no empty, `.` or
/// `..` segment, no NUL or other control character), so names such as `aux.c` or `a:b` that are
/// legal in a repository can still be shown.
pub(crate) fn validate_display_path(path: &str) -> Result<String, SvnError> {
    let rel = path
        .strip_prefix('/')
        .ok_or_else(|| corrupt(format!("repository path is not absolute: {path:?}")))?;
    let bad = !rel.is_empty()
        && (rel.chars().any(char::is_control)
            || rel.split('/').any(|s| matches!(s, "" | "." | "..")));
    if bad {
        return Err(SvnError::new(
            ErrorCode::UnsafePath,
            format!("unsafe repository path from server: {path:?}"),
        ));
    }
    Ok(path.to_owned())
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parses `YYYY-MM-DDThh:mm:ss[.ffffff]Z` (the format Subversion servers use) into microseconds
/// since the Unix epoch.
pub fn parse_timestamp(s: &str) -> Result<Timestamp, SvnError> {
    let bad = || corrupt(format!("invalid timestamp {s:?}"));
    let b = s.trim().as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return Err(bad());
    }
    let num = |r: std::ops::Range<usize>| -> Result<i64, SvnError> {
        let part = b.get(r).ok_or_else(bad)?;
        if !part.iter().all(u8::is_ascii_digit) {
            return Err(bad());
        }
        std::str::from_utf8(part)
            .map_err(|_| bad())?
            .parse::<i64>()
            .map_err(|_| bad())
    };
    let (y, mo, d, h, mi, sec) = (
        num(0..4)?,
        num(5..7)?,
        num(8..10)?,
        num(11..13)?,
        num(14..16)?,
        num(17..19)?,
    );
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return Err(bad());
    }
    let mut idx = 19;
    let mut micros = 0_i64;
    if b.get(idx) == Some(&b'.') {
        idx += 1;
        let start = idx;
        while b.get(idx).is_some_and(u8::is_ascii_digit) {
            idx += 1;
        }
        if idx == start {
            return Err(bad());
        }
        // Keep six fractional digits, pad shorter fractions on the right.
        let mut frac = 0_i64;
        for k in 0..6 {
            frac = frac * 10
                + b.get(start + k)
                    .filter(|_| start + k < idx)
                    .map_or(0, |c| i64::from(c - b'0'));
        }
        micros = frac;
    }
    if b.get(idx) != Some(&b'Z') || idx + 1 != b.len() {
        return Err(bad());
    }
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec;
    secs.checked_mul(1_000_000)
        .and_then(|v| v.checked_add(micros))
        .map(Timestamp)
        .ok_or_else(bad)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_with_micros() {
        assert_eq!(
            parse_timestamp("2026-10-13T08:44:26.336083Z").unwrap(),
            Timestamp(1_791_881_066_336_083)
        );
    }

    #[test]
    fn timestamp_without_fraction_and_epoch() {
        assert_eq!(
            parse_timestamp("1970-01-01T00:00:00Z").unwrap(),
            Timestamp(0)
        );
        assert_eq!(
            parse_timestamp("1970-01-01T00:00:01.5Z").unwrap(),
            Timestamp(1_500_000)
        );
    }

    #[test]
    fn timestamp_rejects_garbage() {
        for s in [
            "",
            "yesterday",
            "2026-13-01T00:00:00Z",
            "2026-10-13T08:44:26",
            "2026-10-13T08:44:26.Z",
        ] {
            assert!(parse_timestamp(s).is_err(), "{s}");
        }
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%20b%C3%A9").unwrap(), "a b\u{e9}");
        assert!(percent_decode("%zz").is_err());
        assert!(percent_decode("%ff").is_err());
        assert!(percent_decode("%4").is_err());
    }

    #[test]
    fn display_paths() {
        assert!(validate_display_path("/trunk/aux.c").is_ok());
        assert!(validate_display_path("trunk").is_err());
        assert!(validate_display_path("/trunk/../x").is_err());
    }
}
