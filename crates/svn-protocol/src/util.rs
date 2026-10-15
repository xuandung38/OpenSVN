//! Small helpers shared by the protocol modules.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
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

/// Decodes a text value that the server may have base64-encoded (`encoding="base64"`, sent when
/// `S:encode-binary-props` is requested and the value is not XML-safe). The result must be UTF-8.
pub(crate) fn decode_text_field(text: String, encoding: Option<&str>) -> Result<String, SvnError> {
    match encoding {
        None => Ok(text),
        Some(_) => {
            let bytes = decode_bytes_field(text, encoding)?;
            String::from_utf8(bytes).map_err(|_| corrupt("base64 field is not UTF-8"))
        }
    }
}

/// Decodes bytes of a value that may be base64-encoded.
pub(crate) fn decode_bytes_field(
    text: String,
    encoding: Option<&str>,
) -> Result<Vec<u8>, SvnError> {
    match encoding {
        None => Ok(text.into_bytes()),
        Some("base64") => {
            let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            STANDARD
                .decode(compact.as_bytes())
                .map_err(|e| corrupt(format!("invalid base64 field: {e}")))
        }
        Some(other) => Err(corrupt(format!("unknown value encoding {other:?}"))),
    }
}

/// A lock token as the engine accepts it: `opaquelocktoken:` followed by printable characters,
/// no whitespace, control characters or angle brackets, at most 256 bytes.
pub(crate) fn validate_lock_token(token: &str) -> Result<String, SvnError> {
    let t = token.trim();
    let ok = t.len() <= 256
        && t.strip_prefix("opaquelocktoken:")
            .is_some_and(|rest| !rest.is_empty())
        && !t
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '<' | '>'));
    if ok {
        Ok(t.to_owned())
    } else {
        Err(SvnError::new(
            ErrorCode::LockTokenInvalid,
            "server sent a malformed lock token",
        ))
    }
}

/// Parses a lower- or upper-case hex digest of exactly `N` bytes.
pub(crate) fn parse_hex<const N: usize>(s: &str) -> Result<[u8; N], SvnError> {
    let s = s.trim();
    if s.len() != N * 2 || !s.is_ascii() {
        return Err(corrupt(format!("invalid checksum {s:?}")));
    }
    let mut out = [0_u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|_| corrupt(format!("invalid checksum {s:?}")))?;
    }
    Ok(out)
}

/// Incremental base64 decoder for text that arrives in arbitrary pieces.
#[derive(Debug, Default)]
pub(crate) struct Base64Stream {
    pending: String,
}

impl Base64Stream {
    /// Adds text (whitespace ignored) and returns every byte that is now decodable.
    pub(crate) fn push(&mut self, text: &str) -> Result<Vec<u8>, SvnError> {
        // The base64 alphabet is ASCII. Rejecting anything else up front also keeps `pending` one
        // byte per character, so the group boundary below is always a char boundary.
        for c in text.chars().filter(|c| !c.is_whitespace()) {
            if !c.is_ascii() {
                return Err(corrupt(format!("invalid base64: non-ASCII character {c:?}")));
            }
            self.pending.push(c);
        }
        let usable = self.pending.len() / 4 * 4;
        if usable == 0 {
            return Ok(Vec::new());
        }
        let rest = self.pending.split_off(usable);
        let chunk = std::mem::replace(&mut self.pending, rest);
        STANDARD
            .decode(chunk.as_bytes())
            .map_err(|e| corrupt(format!("invalid base64: {e}")))
    }

    /// Ends the stream; leftover characters must form a complete group.
    pub(crate) fn finish(&mut self) -> Result<(), SvnError> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            Err(corrupt("truncated base64 data"))
        }
    }
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
    fn base64_stream_rejects_non_ascii_instead_of_splitting_a_char() {
        // Found by the file_revs fuzz target: a BOM after 3 base64 characters put the group
        // boundary inside the multi-byte character and `String::split_off` panicked.
        let mut s = Base64Stream::default();
        assert!(s.push("QUJD\u{feff}=").is_err());
        let mut s = Base64Stream::default();
        assert_eq!(s.push("QU").unwrap(), b"");
        assert!(s.push("J\u{e9}D").is_err());
        let mut s = Base64Stream::default();
        assert_eq!(s.push("QU JD\n").unwrap(), b"ABC");
        s.finish().unwrap();
    }

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
}

#[cfg(test)]
mod codec_tests {
    use super::*;

    #[test]
    fn hex_digests() {
        assert_eq!(parse_hex::<2>("0aFf").unwrap(), [0x0a, 0xff]);
        assert!(parse_hex::<2>("0aF").is_err());
        assert!(parse_hex::<2>("zzzz").is_err());
        assert!(parse_hex::<2>("\u{e9}abc").is_err());
    }

    #[test]
    fn base64_in_odd_pieces() {
        let mut b = Base64Stream::default();
        let mut out = Vec::new();
        for piece in ["U1Z", "OAA", "=", "\n="] {
            out.extend(b.push(piece).unwrap());
        }
        b.finish().unwrap();
        assert_eq!(out, b"SVN\0");
        let mut b = Base64Stream::default();
        b.push("U1Z").unwrap();
        assert!(b.finish().is_err());
        assert!(Base64Stream::default().push("!!!!").is_err());
    }
}
