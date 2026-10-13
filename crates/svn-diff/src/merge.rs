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
// Modified by OpenSVN: merge3 writer and context saver from diff_file.c rewritten for in-memory buffers; configurable labels and fallback EOL.

//! Three-way merge output with Subversion conflict markers.
//!
//! Port of `svn_diff_file_output_merge3` and its context-saver machinery from
//! `libsvn_diff/diff_file.c` (Apache Subversion 1.15.0, Apache License 2.0), operating on
//! in-memory buffers.

use std::collections::VecDeque;

use svn_types::CancelToken;

use crate::control::{Ctl, DiffError};
use crate::diff_core::{Chunk, ChunkKind, diff3};
use crate::tokens::{DiffOptions, detect_eol, split_lines};
use crate::unified::native_eol;

/// How conflicts are rendered (`svn_diff_conflict_display_style_t`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConflictStyle {
    /// `<<<<<<< mine`, `||||||| base`, `=======`, `>>>>>>> theirs` (svn default).
    #[default]
    ModifiedOriginalLatest,
    /// Like the default but without the base section.
    ModifiedLatest,
    /// Conflicts resolved to "mine" with finer-grained markers around real differences.
    ResolvedModifiedLatest,
    /// Take "mine" for every conflict, no markers.
    Modified,
    /// Take "theirs" for every conflict, no markers.
    Latest,
    /// Only conflict regions with three lines of context and `@@` separators.
    OnlyConflicts,
}

/// Names shown in the conflict markers (without the marker characters).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictLabels {
    /// Label of the local version, e.g. `.mine`.
    pub mine: String,
    /// Label of the common ancestor, e.g. `.r12`.
    pub base: String,
    /// Label of the incoming version, e.g. `.r13`.
    pub theirs: String,
}

impl Default for ConflictLabels {
    fn default() -> Self {
        Self {
            mine: ".mine".into(),
            base: ".base".into(),
            theirs: ".theirs".into(),
        }
    }
}

/// Options for [`merge3`].
#[derive(Debug, Clone)]
pub struct MergeOptions {
    /// Comparison options.
    pub diff: DiffOptions,
    /// Conflict rendering.
    pub style: ConflictStyle,
    /// Marker labels.
    pub labels: ConflictLabels,
    /// EOL for marker lines when "mine" contains no line break (svn's `APR_EOL_STR`);
    /// defaults to the native one (`\r\n` on Windows).
    pub fallback_eol: String,
}

impl Default for MergeOptions {
    fn default() -> Self {
        Self {
            diff: DiffOptions::default(),
            style: ConflictStyle::default(),
            labels: ConflictLabels::default(),
            fallback_eol: native_eol().to_owned(),
        }
    }
}

/// A conflicting region, as 0-based line ranges in each input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRegion {
    /// First line in the base and number of lines.
    pub base: (usize, usize),
    /// First line in "mine" and number of lines.
    pub mine: (usize, usize),
    /// First line in "theirs" and number of lines.
    pub theirs: (usize, usize),
}

/// Result of [`merge3`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeResult {
    /// Merged bytes (with markers for conflicts, depending on the style).
    pub text: Vec<u8>,
    /// Conflicting regions, in file order.
    pub conflicts: Vec<ConflictRegion>,
}

const CONTEXT_SIZE: usize = 3;

/// Where output lines go (`output_stream` indirection of the C implementation).
enum Sink {
    /// Straight to the result.
    Real,
    /// Remember the last `CONTEXT_SIZE` lines (leading context).
    Saver,
    /// Print this many more lines (trailing context), then go back to saving.
    Trailing(usize),
}

struct Merger<'a> {
    lines: [Vec<&'a [u8]>; 3],
    current: [i64; 3],
    out: Vec<u8>,
    marker_eol: Vec<u8>,
    labels: [String; 3],
    style: ConflictStyle,
    sink: Sink,
    saved: VecDeque<&'a [u8]>,
    total_writes: usize,
    conflicts: Vec<ConflictRegion>,
}

fn region(c: &(i64, i64)) -> (usize, usize) {
    (
        usize::try_from(c.0).unwrap_or(0),
        usize::try_from(c.1).unwrap_or(0),
    )
}

impl<'a> Merger<'a> {
    /// Writes one source line through the active sink.
    fn write_line(&mut self, line: &'a [u8]) {
        match self.sink {
            Sink::Real => self.out.extend_from_slice(line),
            Sink::Saver => {
                if self.saved.len() == CONTEXT_SIZE {
                    self.saved.pop_front();
                }
                self.saved.push_back(line);
                self.total_writes += 1;
            }
            Sink::Trailing(left) => {
                self.out.extend_from_slice(line);
                if left <= 1 {
                    self.start_saver();
                } else {
                    self.sink = Sink::Trailing(left - 1);
                }
            }
        }
    }

    fn start_saver(&mut self) {
        self.sink = Sink::Saver;
        self.saved.clear();
        self.total_writes = 0;
    }

    /// Writes marker text (only used when the sink is `Real`).
    fn write_raw(&mut self, text: &[u8]) {
        self.out.extend_from_slice(text);
    }

    fn write_raw_eol(&mut self) {
        self.out.extend_from_slice(&self.marker_eol);
    }

    fn output_line(&mut self, idx: usize, skip: bool) {
        let n = self.current[idx];
        self.current[idx] += 1;
        let Some(line) = usize::try_from(n)
            .ok()
            .and_then(|n| self.lines[idx].get(n))
            .copied()
        else {
            return;
        };
        if !skip {
            self.write_line(line);
        }
    }

    fn output_hunk(&mut self, idx: usize, target_line: i64, target_length: i64) {
        while self.current[idx] < target_line {
            self.output_line(idx, true);
        }
        let end = target_line + target_length;
        while self.current[idx] < end {
            self.output_line(idx, false);
        }
    }

    fn marker(&mut self, label_idx: usize, prefix: &str) {
        let text = format!("{prefix} {}", self.labels[label_idx]);
        self.write_raw(text.as_bytes());
        self.write_raw_eol();
    }

    fn conflict_with_context(&mut self, c: &Chunk) {
        if matches!(self.sink, Sink::Saver) {
            if self.total_writes > CONTEXT_SIZE {
                self.write_raw(b"@@\n");
            }
            let saved: Vec<&[u8]> = self.saved.iter().copied().collect();
            for line in saved {
                self.out.extend_from_slice(line);
            }
        }
        self.sink = Sink::Real;
        self.context_marker_label(0, c.modified_start, c.modified_length);
        self.output_hunk(1, c.modified_start, c.modified_length);
        self.context_marker_label(1, c.original_start, c.original_length);
        self.output_hunk(0, c.original_start, c.original_length);
        self.write_raw(b"=======");
        self.write_raw_eol();
        self.output_hunk(2, c.latest_start, c.latest_length);
        self.context_marker_label(2, c.latest_start, c.latest_length);
        self.sink = Sink::Trailing(CONTEXT_SIZE);
    }

    /// `output_conflict_with_context_marker`: marker, label and 1-based position.
    fn context_marker_label(&mut self, label_idx: usize, start: i64, length: i64) {
        let prefix = ["<<<<<<<", "|||||||", ">>>>>>>"][label_idx];
        let label = &self.labels[label_idx];
        let text = if length == 1 {
            format!("{prefix} {label} ({})", start + 1)
        } else {
            format!("{prefix} {label} ({},{length})", start + 1)
        };
        self.write_raw(text.as_bytes());
        self.write_raw_eol();
    }

    fn conflict(&mut self, c: &Chunk, resolved: Option<&[Chunk]>) {
        let mut style = self.style;
        if style == ConflictStyle::OnlyConflicts {
            self.conflicts.push(ConflictRegion {
                base: region(&(c.original_start, c.original_length)),
                mine: region(&(c.modified_start, c.modified_length)),
                theirs: region(&(c.latest_start, c.latest_length)),
            });
            self.conflict_with_context(c);
            return;
        }
        if style == ConflictStyle::ResolvedModifiedLatest {
            if let Some(sub) = resolved.filter(|s| !s.is_empty()) {
                self.output_chunks(sub, false);
                return;
            }
            style = ConflictStyle::ModifiedLatest;
        }
        self.conflicts.push(ConflictRegion {
            base: region(&(c.original_start, c.original_length)),
            mine: region(&(c.modified_start, c.modified_length)),
            theirs: region(&(c.latest_start, c.latest_length)),
        });
        match style {
            ConflictStyle::ModifiedLatest | ConflictStyle::ModifiedOriginalLatest => {
                self.marker(0, "<<<<<<<");
                self.output_hunk(1, c.modified_start, c.modified_length);
                if style == ConflictStyle::ModifiedOriginalLatest {
                    self.marker(1, "|||||||");
                    self.output_hunk(0, c.original_start, c.original_length);
                }
                self.write_raw(b"=======");
                self.write_raw_eol();
                self.output_hunk(2, c.latest_start, c.latest_length);
                self.marker(2, ">>>>>>>");
            }
            ConflictStyle::Modified => self.output_hunk(1, c.modified_start, c.modified_length),
            ConflictStyle::Latest => self.output_hunk(2, c.latest_start, c.latest_length),
            ConflictStyle::ResolvedModifiedLatest | ConflictStyle::OnlyConflicts => {}
        }
    }

    /// `svn_diff_output2` over the diff3 vtable.  `top` is true for the outermost list.
    fn output_chunks(&mut self, chunks: &[Chunk], top: bool) {
        for c in chunks {
            match c.kind {
                ChunkKind::Common | ChunkKind::DiffModified | ChunkKind::DiffCommon => {
                    self.output_hunk(1, c.modified_start, c.modified_length);
                }
                ChunkKind::DiffLatest => self.output_hunk(2, c.latest_start, c.latest_length),
                ChunkKind::Conflict => {
                    let sub = if top {
                        Some(c.resolved.as_slice())
                    } else {
                        None
                    };
                    self.conflict(c, sub);
                }
            }
        }
    }
}

/// Merges `mine` and `theirs` relative to `base` with Subversion's diff3 semantics.
///
/// Works on bytes; line terminators are `\n`, `\r\n` or `\r`.  Conflict marker lines use the
/// EOL style of `mine` (falling back to `opts.fallback_eol`).
///
/// `conflicts.is_empty()` is the "merged cleanly" signal; regions are also recorded for the
/// `Modified` and `Latest` styles, which resolve them without markers.
///
/// Memory is roughly 100 bytes per line over all three inputs plus the buffers themselves, and
/// time grows with size times edit distance.  Callers should run this on a blocking thread and
/// skip auto-merge (report a conflict instead) above a size threshold such as 64 MB or about
/// one million lines, or set `opts.diff.max_cost`.
///
/// # Errors
/// [`DiffError::Cancelled`] when `cancel` fires, [`DiffError::TooComplex`] when
/// `opts.diff.max_cost` is exceeded.
pub fn merge3(
    base: &[u8],
    mine: &[u8],
    theirs: &[u8],
    opts: &MergeOptions,
    cancel: &CancelToken,
) -> Result<MergeResult, DiffError> {
    let mut ctl = Ctl::new(cancel, opts.diff.max_cost);
    let chunks = diff3(base, mine, theirs, &opts.diff, &mut ctl)?;
    let mut m = Merger {
        lines: [split_lines(base), split_lines(mine), split_lines(theirs)],
        current: [0; 3],
        out: Vec::with_capacity(mine.len().max(theirs.len())),
        marker_eol: detect_eol(mine)
            .map_or_else(|| opts.fallback_eol.as_bytes().to_vec(), <[u8]>::to_vec),
        labels: [
            opts.labels.mine.clone(),
            opts.labels.base.clone(),
            opts.labels.theirs.clone(),
        ],
        style: opts.style,
        sink: Sink::Real,
        saved: VecDeque::new(),
        total_writes: 0,
        conflicts: Vec::new(),
    };
    if opts.style == ConflictStyle::OnlyConflicts {
        m.start_saver();
    }
    m.output_chunks(&chunks, true);
    Ok(MergeResult {
        text: m.out,
        conflicts: m.conflicts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> MergeOptions {
        MergeOptions {
            labels: ConflictLabels {
                mine: ".mine".into(),
                base: ".r1".into(),
                theirs: ".r2".into(),
            },
            fallback_eol: "\n".into(),
            ..MergeOptions::default()
        }
    }

    fn run(base: &[u8], mine: &[u8], theirs: &[u8], o: &MergeOptions) -> MergeResult {
        merge3(base, mine, theirs, o, &CancelToken::new()).unwrap_or_else(|_| MergeResult {
            text: Vec::new(),
            conflicts: Vec::new(),
        })
    }

    #[test]
    fn clean_merge_of_distant_edits() {
        let r = run(
            b"a\nb\nc\nd\ne\nf\ng\n",
            b"A\nb\nc\nd\ne\nf\ng\n",
            b"a\nb\nc\nd\ne\nf\nG\n",
            &opts(),
        );
        assert_eq!(r.text, b"A\nb\nc\nd\ne\nf\nG\n");
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn conflict_markers() {
        let r = run(b"a\n", b"b\n", b"c\n", &opts());
        assert_eq!(
            String::from_utf8_lossy(&r.text),
            "<<<<<<< .mine\nb\n||||||| .r1\na\n=======\nc\n>>>>>>> .r2\n"
        );
        assert_eq!(r.conflicts.len(), 1);
    }

    #[test]
    fn identical_edits_do_not_conflict() {
        let r = run(b"a\nb\n", b"a\nX\n", b"a\nX\n", &opts());
        assert_eq!(r.text, b"a\nX\n");
        assert!(r.conflicts.is_empty());
    }
}
