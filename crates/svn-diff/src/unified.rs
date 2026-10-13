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
// Modified by OpenSVN: unified writer from diff_file.c and util.c rewritten for in-memory buffers; configurable EOL.

//! Unified diff output identical to `svn diff`.
//!
//! Port of `svn_diff_file_output_unified4` and helpers from `libsvn_diff/diff_file.c` and
//! `libsvn_diff/util.c` (Apache Subversion 1.15.0, Apache License 2.0), operating on
//! in-memory buffers.

use svn_types::CancelToken;

use crate::control::{Ctl, DiffError};
use crate::diff_core::{Chunk, ChunkKind, diff2};
use crate::tokens::{DiffOptions, split_lines};

/// Default number of context lines (`SVN_DIFF__UNIFIED_CONTEXT_SIZE`).
pub const DEFAULT_CONTEXT_SIZE: usize = 3;

/// Options for [`unified`].
#[derive(Debug, Clone)]
pub struct UnifiedOptions {
    /// Comparison options (whitespace / EOL handling).
    pub diff: DiffOptions,
    /// Context lines around each change.
    pub context_size: usize,
    /// Text after `--- ` (typically `path<TAB>(revision N)`).
    pub original_header: String,
    /// Text after `+++ ` (typically `path<TAB>(working copy)`).
    pub modified_header: String,
    /// Hunk delimiter; `svn diff` uses `@@`, property diffs use `##`.
    pub hunk_delimiter: String,
    /// Line ending for header lines and the `\ No newline` marker (svn's `APR_EOL_STR`).
    /// Defaults to the native one: `\r\n` on Windows, `\n` elsewhere.
    pub eol: String,
}

impl Default for UnifiedOptions {
    fn default() -> Self {
        Self {
            diff: DiffOptions::default(),
            context_size: DEFAULT_CONTEXT_SIZE,
            original_header: String::new(),
            modified_header: String::new(),
            hunk_delimiter: "@@".to_owned(),
            eol: native_eol().to_owned(),
        }
    }
}

/// The platform line ending svn uses for generated lines (`APR_EOL_STR`).
#[must_use]
pub const fn native_eol() -> &'static str {
    if cfg!(windows) { "\r\n" } else { "\n" }
}

/// Builds the `Index:` banner `svn diff` prints before each file diff, with `eol` after
/// each of its two lines (use [`native_eol`] to match svn on the current OS).
#[must_use]
pub fn index_header(path: &str, eol: &str) -> String {
    format!("Index: {path}{eol}{}{eol}", "=".repeat(67))
}

/// Returns true when the two inputs differ under `opts`.
///
/// # Errors
/// [`DiffError::Cancelled`] or [`DiffError::TooComplex`].
pub fn contains_diffs(
    original: &[u8],
    modified: &[u8],
    opts: &DiffOptions,
    cancel: &CancelToken,
) -> Result<bool, DiffError> {
    let mut ctl = Ctl::new(cancel, opts.max_cost);
    Ok(diff2(original, modified, opts, &mut ctl)?
        .iter()
        .any(|c| c.kind != ChunkKind::Common))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LineType {
    Skip,
    Context,
    Delete,
    Insert,
}

struct Writer<'a> {
    lines: [Vec<&'a [u8]>; 2],
    current_line: [i64; 2],
    hunk_start: [i64; 2],
    hunk_length: [i64; 2],
    hunk: Vec<u8>,
    out: Vec<u8>,
    context_size: i64,
    delimiter: &'a str,
    eol: &'a str,
}

impl Writer<'_> {
    fn output_line(&mut self, kind: LineType, idx: usize) {
        // Lazily advance even at EOF so trailing context at EOF is faked correctly.
        let n = self.current_line[idx];
        self.current_line[idx] += 1;
        let Some(line) = usize::try_from(n)
            .ok()
            .and_then(|n| self.lines[idx].get(n))
            .copied()
        else {
            return;
        };
        match kind {
            LineType::Context => {
                self.hunk.push(b' ');
                self.hunk_length[0] += 1;
                self.hunk_length[1] += 1;
            }
            LineType::Delete => {
                self.hunk.push(b'-');
                self.hunk_length[0] += 1;
            }
            LineType::Insert => {
                self.hunk.push(b'+');
                self.hunk_length[1] += 1;
            }
            LineType::Skip => return,
        }
        self.hunk.extend_from_slice(line);
        if !matches!(line.last(), Some(b'\n' | b'\r')) {
            self.hunk.extend_from_slice(self.eol.as_bytes());
            self.hunk.extend_from_slice(b"\\ No newline at end of file");
            self.hunk.extend_from_slice(self.eol.as_bytes());
        }
    }

    fn range(&mut self, source: usize, kind: LineType, until: i64) {
        while self.current_line[source] < until {
            self.output_line(kind, source);
        }
    }

    fn flush_hunk(&mut self) {
        if self.hunk.is_empty() {
            return;
        }
        let target = self.hunk_start[0] + self.hunk_length[0] + self.context_size;
        self.range(0, LineType::Context, target);

        let mut old_start = self.hunk_start[0];
        let mut new_start = self.hunk_start[1];
        if self.hunk_length[0] != 0 {
            old_start += 1;
        }
        if self.hunk_length[1] != 0 {
            new_start += 1;
        }
        let d = self.delimiter;
        let mut header = format!("{d} -{old_start}");
        if self.hunk_length[0] != 1 {
            header.push_str(&format!(",{}", self.hunk_length[0]));
        }
        header.push_str(&format!(" +{new_start}"));
        if self.hunk_length[1] != 1 {
            header.push_str(&format!(",{}", self.hunk_length[1]));
        }
        header.push_str(&format!(" {d}{}", self.eol));
        self.out.extend_from_slice(header.as_bytes());
        self.out.append(&mut self.hunk);

        self.hunk_length = [0, 0];
        self.hunk_start = [0, 0];
    }

    fn diff_modified(&mut self, c: &Chunk) {
        let (os, ol) = (c.original_start, c.original_length);
        let (ms, ml) = (c.modified_start, c.modified_length);
        let context_prefix = self.context_size.min(os);

        let mut init_hunk = false;
        let prev_context_end = if self.hunk_length[0] > 0 || self.hunk_length[1] > 0 {
            self.hunk_start[0] + self.hunk_length[0] + self.context_size
        } else {
            if self.hunk_start[0] == 0 && (ol > 0 || ml > 0) {
                init_hunk = true;
            }
            -1
        };

        let new_hunk_start = os - context_prefix;
        if self.current_line[0] < new_hunk_start && prev_context_end <= new_hunk_start {
            self.flush_hunk();
            init_hunk = true;
        } else if self.hunk_length[0] > 0 || self.hunk_length[1] > 0 {
            // Extend the current hunk with the context preceding the change.
            self.range(0, LineType::Context, os);
        }

        self.range(0, LineType::Skip, os - context_prefix);
        if init_hunk {
            self.hunk_start[0] = os - context_prefix;
            self.hunk_start[1] = ms - context_prefix;
        }
        self.range(1, LineType::Skip, ms);
        self.range(0, LineType::Context, os);
        self.range(0, LineType::Delete, os + ol);
        self.range(1, LineType::Insert, ms + ml);
    }
}

/// Produces the `svn diff` unified output (headers + hunks) for `original` -> `modified`.
///
/// Returns an empty buffer when the inputs do not differ under `opts.diff`.  The result is
/// raw bytes because file content need not be UTF-8.
///
/// Run on a blocking thread for large inputs: cost grows with size times edit distance.
///
/// # Errors
/// [`DiffError::Cancelled`] when `cancel` fires, [`DiffError::TooComplex`] when
/// `opts.diff.max_cost` is exceeded.
pub fn unified(
    original: &[u8],
    modified: &[u8],
    opts: &UnifiedOptions,
    cancel: &CancelToken,
) -> Result<Vec<u8>, DiffError> {
    let mut ctl = Ctl::new(cancel, opts.diff.max_cost);
    let chunks = diff2(original, modified, &opts.diff, &mut ctl)?;
    if chunks.iter().all(|c| c.kind == ChunkKind::Common) {
        return Ok(Vec::new());
    }
    let mut w = Writer {
        lines: [split_lines(original), split_lines(modified)],
        current_line: [0, 0],
        hunk_start: [0, 0],
        hunk_length: [0, 0],
        hunk: Vec::new(),
        out: Vec::new(),
        context_size: i64::try_from(opts.context_size).unwrap_or(i64::MAX / 4),
        delimiter: &opts.hunk_delimiter,
        eol: &opts.eol,
    };
    w.out.extend_from_slice(
        format!(
            "--- {}{eol}+++ {}{eol}",
            opts.original_header,
            opts.modified_header,
            eol = opts.eol
        )
        .as_bytes(),
    );
    for c in chunks.iter().filter(|c| c.kind == ChunkKind::DiffModified) {
        w.diff_modified(c);
    }
    w.flush_hunk();
    Ok(w.out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> UnifiedOptions {
        UnifiedOptions {
            eol: "\n".into(),
            original_header: "f\t(revision 1)".into(),
            modified_header: "f\t(working copy)".into(),
            ..UnifiedOptions::default()
        }
    }

    #[test]
    fn simple_change() {
        let out =
            unified(b"a\nb\nc\n", b"a\nB\nc\n", &opts(), &CancelToken::new()).unwrap_or_default();
        assert_eq!(
            String::from_utf8_lossy(&out),
            "--- f\t(revision 1)\n+++ f\t(working copy)\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n"
        );
    }

    #[test]
    fn no_newline_marker() {
        let out = unified(b"a\nb", b"a\nc", &opts(), &CancelToken::new()).unwrap_or_default();
        assert!(
            String::from_utf8_lossy(&out)
                .ends_with("-b\n\\ No newline at end of file\n+c\n\\ No newline at end of file\n")
        );
    }

    #[test]
    fn crlf_eol_applies_to_generated_lines_only() {
        let o = UnifiedOptions {
            eol: "\r\n".into(),
            ..opts()
        };
        let out = unified(b"a\nb", b"a\nc", &o, &CancelToken::new()).unwrap_or_default();
        let text = String::from_utf8_lossy(&out).into_owned();
        assert!(text.starts_with(
            "--- f\t(revision 1)\r\n+++ f\t(working copy)\r\n@@ -1,2 +1,2 @@\r\n a\n"
        ));
        assert!(text.contains("-b\r\n\\ No newline at end of file\r\n"));
    }

    #[test]
    fn identical_is_empty() {
        assert!(
            unified(b"x\n", b"x\n", &opts(), &CancelToken::new())
                .unwrap_or_default()
                .is_empty()
        );
    }

    #[test]
    fn index_banner() {
        assert_eq!(
            index_header("f", "\n"),
            format!("Index: f\n{}\n", "=".repeat(67))
        );
    }
}
