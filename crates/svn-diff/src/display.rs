//! Display-oriented diff hunks, built on the same LCS engine (with identical prefix/suffix
//! skipping) as [`crate::unified`], so UI and `svn diff` agree on what changed.

use svn_types::CancelToken;

use crate::control::{Ctl, DiffError};
use crate::diff_core::{ChunkKind, diff2};
use crate::tokens::{DiffOptions, split_lines};

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
///
/// # Errors
/// [`DiffError::Cancelled`] when `cancel` fires, [`DiffError::TooComplex`] when
/// `opts.diff.max_cost` is exceeded.
pub fn diff(
    old: &[u8],
    new: &[u8],
    opts: &DisplayOptions,
    cancel: &CancelToken,
) -> Result<Vec<Hunk>, DiffError> {
    let old_lines = split_lines(old);
    let new_lines = split_lines(new);
    let mut ctl = Ctl::new(cancel, opts.diff.max_cost);

    // Raw changes as (old start, old end, new start, new end), 0-based half-open.
    let to_usize = |v: i64| usize::try_from(v).unwrap_or(0);
    let changes: Vec<(usize, usize, usize, usize)> = diff2(old, new, &opts.diff, &mut ctl)?
        .iter()
        .filter(|c| c.kind == ChunkKind::DiffModified)
        .map(|c| {
            let (os, ms) = (to_usize(c.original_start), to_usize(c.modified_start));
            (
                os,
                os + to_usize(c.original_length),
                ms,
                ms + to_usize(c.modified_length),
            )
        })
        .collect();

    let gap = opts.context.saturating_mul(2);
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut i = 0;
    while i < changes.len() {
        // Group changes whose gap is within 2 * context.
        let mut j = i;
        while j + 1 < changes.len() && changes[j + 1].0 - changes[j].1 <= gap {
            j += 1;
        }
        let (first, last) = (changes[i], changes[j]);
        let lead = opts.context.min(first.0);
        let trail = opts.context.min(old_lines.len().saturating_sub(last.1));
        let old_start = first.0 - lead;
        let new_start = first.2 - lead;
        let mut lines = Vec::new();
        let ctx = |lines: &mut Vec<HunkLine>, from: usize, to: usize| {
            for l in old_lines.get(from..to).unwrap_or(&[]) {
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
            for l in old_lines.get(os..oe).unwrap_or(&[]) {
                lines.push(HunkLine {
                    kind: LineKind::Removed,
                    text: l.to_vec(),
                });
            }
            for l in new_lines.get(ns..ne).unwrap_or(&[]) {
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
    Ok(hunks)
}
