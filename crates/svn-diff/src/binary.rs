// ====================================================================
//    Licensed to the Apache Software Foundation (ASF) under one
//    or more contributor license agreements.  See the NOTICE file
//    distributed with this work for additional information
//    regarding copyright ownership.  The ASF licenses this file
//    to you under the Apache License, Version 2.0 (the
//    "License"); you may not use this file except in compliance
//    with the License.  You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
//    Unless required by applicable law or agreed to in writing,
//    software distributed under the License is distributed on an
//    "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
//    KIND, either express or implied.  See the License for the
//    specific language governing permissions and limitations
//    under the License.
// ====================================================================
//
// Modified by OpenSVN: mime and binary-data rules from validate.c and io.c reimplemented in Rust.

//! Binary detection and display-encoding sniffing.
//!
//! Two different questions are kept apart, as in Subversion:
//!
//! * [`diff_as_binary_by_mime`]: does diff/merge treat a *versioned* file as binary?  Only the
//!   `svn:mime-type` property decides (`svn_mime_type_is_binary`, `validate.c`).  A versioned
//!   file without that property is always text, even if it contains NUL bytes.
//! * [`detect_binary_for_add`]: the content heuristic svn runs when *adding* a file to pick an
//!   initial `svn:mime-type` (`svn_io_is_binary_data`, `io.c`).  Never use it to decide
//!   diff/merge behaviour of an existing file.

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

/// Whether diff and merge must treat a versioned file as binary, given its `svn:mime-type`.
///
/// `None` (no property) is text.  With a value, only the part before the first `;` or space is
/// examined: anything not starting with `text/` is binary, except `image/x-xbitmap` and
/// `image/x-xpixmap`.  No trimming is done, exactly like svn.
#[must_use]
pub fn diff_as_binary_by_mime(mime: Option<&str>) -> bool {
    let Some(m) = mime else {
        return false;
    };
    let end = m.find([';', ' ']).unwrap_or(m.len());
    let kind = &m[..end];
    !(m.starts_with("text/") || kind == "image/x-xbitmap" || kind == "image/x-xpixmap")
}

/// Add-time heuristic (`svn_io_is_binary_data` on the first 1024 bytes): a NUL byte, or more
/// than 85 percent unusual control/high bytes, means binary.
#[must_use]
pub fn detect_binary_for_add(bytes: &[u8]) -> bool {
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
    fn mime_rules_follow_svn() {
        assert!(!diff_as_binary_by_mime(None));
        assert!(!diff_as_binary_by_mime(Some("text/plain")));
        assert!(!diff_as_binary_by_mime(Some("text/plain; charset=utf-8")));
        assert!(diff_as_binary_by_mime(Some("application/octet-stream")));
        assert!(!diff_as_binary_by_mime(Some("image/x-xbitmap")));
        assert!(!diff_as_binary_by_mime(Some("image/x-xbitmap; charset=x")));
        assert!(diff_as_binary_by_mime(Some("image/x-xbitmapx")));
        assert!(diff_as_binary_by_mime(Some(" text/plain")));
        assert!(diff_as_binary_by_mime(Some("image/png")));
    }

    #[test]
    fn add_heuristic() {
        assert!(detect_binary_for_add(b"ab\0c"));
        assert!(!detect_binary_for_add(b"hello\nworld\n"));
        assert!(!detect_binary_for_add(&[0xEF, 0xBB, 0xBF]));
        assert!(detect_binary_for_add(&[0x80; 100]));
        assert!(!detect_binary_for_add(b""));
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
        assert_eq!(detect_encoding("h\u{e9}llo".as_bytes()), TextEncoding::Utf8);
        assert_eq!(detect_encoding(&[0x68, 0xE9]), TextEncoding::Latin1);
    }
}
