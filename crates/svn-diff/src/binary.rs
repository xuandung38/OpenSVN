//! Binary detection and display-encoding sniffing.
//!
//! The binary rules follow `svn_mime_type_is_binary` and `svn_io_is_binary_data`
//! (Apache Subversion 1.15.0, Apache License 2.0).

/// Encoding guess for display purposes only; merging always works on bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEncoding {
    /// UTF-8, with or without BOM.
    Utf8,
    /// UTF-16 little endian (BOM `FF FE`).
    Utf16Le,
    /// UTF-16 big endian (BOM `FE FF`).
    Utf16Be,
    /// Anything else; render as ISO-8859-1.
    Latin1,
}

/// Reports whether `bytes` should be treated as binary.
///
/// With a `svn:mime-type`, anything not starting with `text/` is binary (except the two X
/// bitmap types).  Without one, the first 1024 bytes are inspected: a NUL byte, or more
/// than 85 percent unusual control/high bytes, means binary.
#[must_use]
pub fn is_binary(bytes: &[u8], mime: Option<&str>) -> bool {
    if let Some(m) = mime {
        let m = m.trim();
        return !(m.starts_with("text/") || m == "image/x-xbitmap" || m == "image/x-xpixmap");
    }
    is_binary_data(bytes)
}

/// `svn_io_is_binary_data` on at most the first 1024 bytes.
fn is_binary_data(bytes: &[u8]) -> bool {
    let block = bytes.get(..1024).unwrap_or(bytes);
    if block == [0xEF, 0xBB, 0xBF] {
        return false; // empty UTF-8 file consisting only of the BOM
    }
    if block.is_empty() {
        return false;
    }
    let mut odd = 0usize;
    for &b in block {
        if b == 0 {
            return true;
        }
        if b < 0x07 || (b > 0x0D && b < 0x20) || b > 0x7F {
            odd += 1;
        }
    }
    odd * 1000 / block.len() > 850
}

/// Guesses the display encoding: BOMs first, then UTF-8 validity, else Latin-1.
#[must_use]
pub fn detect_encoding(bytes: &[u8]) -> TextEncoding {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        TextEncoding::Utf16Le
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        TextEncoding::Utf16Be
    } else if std::str::from_utf8(bytes).is_ok() {
        TextEncoding::Utf8
    } else {
        TextEncoding::Latin1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_rules() {
        assert!(!is_binary(b"\0", Some("text/plain")));
        assert!(is_binary(b"abc", Some("application/octet-stream")));
        assert!(!is_binary(b"abc", Some("image/x-xbitmap")));
    }

    #[test]
    fn heuristic() {
        assert!(is_binary(b"ab\0c", None));
        assert!(!is_binary(b"hello\nworld\n", None));
        assert!(!is_binary(&[0xEF, 0xBB, 0xBF], None));
        assert!(is_binary(&[0x80; 100], None));
        assert!(!is_binary(b"", None));
    }

    #[test]
    fn encodings() {
        assert_eq!(
            detect_encoding(&[0xFF, 0xFE, 0x41, 0]),
            TextEncoding::Utf16Le
        );
        assert_eq!(
            detect_encoding(&[0xFE, 0xFF, 0, 0x41]),
            TextEncoding::Utf16Be
        );
        assert_eq!(detect_encoding("héllo".as_bytes()), TextEncoding::Utf8);
        assert_eq!(detect_encoding(&[0x68, 0xE9]), TextEncoding::Latin1);
    }
}
