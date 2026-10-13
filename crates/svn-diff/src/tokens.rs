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
// Modified by OpenSVN: tokenizer from diff_file.c and normalization from util.c, rewritten for in-memory byte slices.

//! Line tokenizer and whitespace/EOL normalization.
//!
//! Behaviour follows `libsvn_diff/diff_file.c` (token reading) and
//! `libsvn_diff/util.c` (`svn_diff__normalize_buffer`) from Apache Subversion 1.15.0
//! (Apache License 2.0).  A line token includes its terminator; terminators are
//! `\n`, `\r\n` and a lone `\r`.

use std::borrow::Cow;

/// How runs of whitespace are compared (`svn diff -x -b / -w`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IgnoreSpace {
    /// Compare whitespace literally.
    #[default]
    None,
    /// Treat any run of whitespace as a single space (`-b`).
    Change,
    /// Ignore all whitespace (`-w`).
    All,
}

/// Comparison options shared by every diff entry point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffOptions {
    /// Whitespace handling.
    pub ignore_space: IgnoreSpace,
    /// Treat `\n`, `\r\n` and `\r` as equal line terminators.
    pub ignore_eol_style: bool,
    /// Optional work budget in LCS steps; exceeding it yields `DiffError::TooComplex`.
    /// `None` means unbounded (still cancellable).
    pub max_cost: Option<u64>,
}

impl DiffOptions {
    fn is_noop(&self) -> bool {
        self.ignore_space == IgnoreSpace::None && !self.ignore_eol_style
    }
}

/// Splits `data` into lines, each keeping its terminator.
///
/// A trailing fragment without terminator becomes the last token.
#[must_use]
pub fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < data.len() {
        match data.get(i) {
            Some(b'\n') => {
                i += 1;
                lines.push(&data[start..i]);
                start = i;
            }
            Some(b'\r') => {
                i += 1;
                if data.get(i) == Some(&b'\n') {
                    i += 1;
                }
                lines.push(&data[start..i]);
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < data.len() {
        lines.push(&data[start..]);
    }
    lines
}

/// Returns the EOL sequence of the first line terminator in `data`, if any.
///
/// Mirrors `svn_eol__detect_eol`.
#[must_use]
pub fn detect_eol(data: &[u8]) -> Option<&'static [u8]> {
    let idx = data.iter().position(|&b| b == b'\n' || b == b'\r')?;
    if data.get(idx) == Some(&b'\n') {
        Some(b"\n")
    } else if data.get(idx + 1) == Some(&b'\n') {
        Some(b"\r\n")
    } else {
        Some(b"\r")
    }
}

/// `svn_ctype_isspace`: space, \t, \n, \v, \f, \r.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Normalizes one line token for comparison (`svn_diff__normalize_buffer`).
///
/// Borrowed when no change is required.
pub(crate) fn normalize<'a>(line: &'a [u8], opts: &DiffOptions) -> Cow<'a, [u8]> {
    if opts.is_noop() {
        return Cow::Borrowed(line);
    }
    #[derive(PartialEq)]
    enum State {
        Normal,
        Whitespace,
        Cr,
    }
    let mut out = Vec::with_capacity(line.len());
    let mut state = State::Normal;
    for &b in line {
        match b {
            b'\r' => {
                out.push(if opts.ignore_eol_style { b'\n' } else { b'\r' });
                state = State::Cr;
            }
            b'\n' => {
                if !(state == State::Cr && opts.ignore_eol_style) {
                    out.push(b'\n');
                }
                state = State::Normal;
            }
            _ if is_space(b) && opts.ignore_space != IgnoreSpace::None => {
                if state != State::Whitespace && opts.ignore_space == IgnoreSpace::Change {
                    out.push(b' ');
                }
                state = State::Whitespace;
            }
            _ => {
                out.push(b);
                state = State::Normal;
            }
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_all_eol_styles() {
        let lines = split_lines(b"a\nb\r\nc\rd");
        assert_eq!(lines, vec![&b"a\n"[..], b"b\r\n", b"c\r", b"d"]);
        assert!(split_lines(b"").is_empty());
    }

    #[test]
    fn detects_eol() {
        assert_eq!(detect_eol(b"x\r\ny"), Some(&b"\r\n"[..]));
        assert_eq!(detect_eol(b"x\ry"), Some(&b"\r"[..]));
        assert_eq!(detect_eol(b"x\ny"), Some(&b"\n"[..]));
        assert_eq!(detect_eol(b"xy"), None);
    }

    #[test]
    fn normalizes_space_and_eol() {
        let o = DiffOptions {
            ignore_space: IgnoreSpace::Change,
            ignore_eol_style: true,
            max_cost: None,
        };
        assert_eq!(&*normalize(b"a \t b\r\n", &o), b"a b\n");
        let o = DiffOptions {
            ignore_space: IgnoreSpace::All,
            ignore_eol_style: false,
            max_cost: None,
        };
        assert_eq!(&*normalize(b" a \t b\r\n", &o), b"ab\r\n");
    }
}
