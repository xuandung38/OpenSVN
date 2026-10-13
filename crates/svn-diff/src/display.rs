//! Display-oriented diff hunks (histogram diff via `imara-diff`).
//!
//! Hunk boundaries may differ from `svn diff`; use [`crate::unified`] when byte-exact svn
//! output matters.  The content (which lines are removed/inserted) is always a valid edit
//! script from `old` to `new`.

use imara_diff::{Algorithm, Diff, InternedInput};

use crate::tokens::{DiffOptions, normalize, split_lines};

/// Options for [`diff`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayOptions {
    /// Comparison options.
    pub diff: DiffOptions,
    /// Context lines kept around each change.
    pub context: usize,
}

impl Default for DisplayOptions {
    fn default() -> Self {
        Self {
            diff: DiffOptions::default(),
            context: 3,
        }
    }
}

/// Kind of a [`HunkLine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Present in both versions (text taken from the old version).
    Context,
    /// Only in the old version.
    Removed,
    /// Only in the new version.
    Added,
}

/// One line of a hunk, terminator included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkLine {
    /// Line classification.
    pub kind: LineKind,
    /// Raw line bytes.
    pub text: Vec<u8>,
}

/// A group of nearby changes with context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// 0-based first line in the old version.
    pub old_start: usize,
    /// Number of old lines covered.
    pub old_len: usize,
    /// 0-based first line in the new version.
    pub new_start: usize,
    /// Number of new lines covered.
    pub new_len: usize,
    /// Lines in display order.
    pub lines: Vec<HunkLine>,
}

/// Computes display hunks between `old` and `new`.
#[must_use]
pub fn diff(old: &[u8], new: &[u8], opts: &DisplayOptions) -> Vec<Hunk> {
    let old_lines = split_lines(old);
    let new_lines = split_lines(new);
    let mut input = InternedInput::default();
    input.update_before(
        old_lines
            .iter()
            .map(|l| normalize(l, &opts.diff).into_owned()),
    );
    input.update_after(
        new_lines
            .iter()
            .map(|l| normalize(l, &opts.diff).into_owned()),
    );
    let algo = Diff::compute(Algorithm::Histogram, &input);

    // Raw changes as (old range, new range).
    let changes: Vec<(usize, usize, usize, usize)> = algo
        .hunks()
        .map(|h| {
            (
                h.before.start as usize,
                h.before.end as usize,
                h.after.start as usize,
                h.after.end as usize,
            )
        })
        .collect();

    let mut hunks: Vec<Hunk> = Vec::new();
    let mut i = 0;
    while i < changes.len() {
        // Group changes whose gap is within 2 * context.
        let mut j = i;
        while j + 1 < changes.len() && changes[j + 1].0 - changes[j].1 <= 2 * opts.context {
            j += 1;
        }
        let (first, last) = (changes[i], changes[j]);
        let lead = opts.context.min(first.0);
        let trail = opts.context.min(old_lines.len() - last.1);
        let old_start = first.0 - lead;
        let new_start = first.2 - lead;
        let mut lines = Vec::new();
        let ctx = |lines: &mut Vec<HunkLine>, from: usize, to: usize| {
            for l in &old_lines[from..to] {
                lines.push(HunkLine {
                    kind: LineKind::Context,
                    text: l.to_vec(),
                });
            }
        };
        ctx(&mut lines, old_start, first.0);
        for k in i..=j {
            let (os, oe, ns, ne) = changes[k];
            if k > i {
                ctx(&mut lines, changes[k - 1].1, os);
            }
            for l in &old_lines[os..oe] {
                lines.push(HunkLine {
                    kind: LineKind::Removed,
                    text: l.to_vec(),
                });
            }
            for l in &new_lines[ns..ne] {
                lines.push(HunkLine {
                    kind: LineKind::Added,
                    text: l.to_vec(),
                });
            }
        }
        ctx(&mut lines, last.1, last.1 + trail);
        let old_len = last.1 + trail - old_start;
        let new_len = last.3 + trail - new_start;
        hunks.push(Hunk {
            old_start,
            old_len,
            new_start,
            new_len,
            lines,
        });
        i = j + 1;
    }
    hunks
}
