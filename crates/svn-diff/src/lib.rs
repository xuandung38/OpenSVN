//! Subversion-compatible text diff, unified output and three-way merge.
//!
//! * [`unified`] and [`merge3`] port the algorithms of Apache Subversion's `libsvn_diff`
//!   (1.15.0, Apache License 2.0) so that their output matches `svn diff` and the conflict
//!   files produced by `svn update` byte for byte.
//! * [`diff`] gives display hunks from the same LCS engine, so the UI agrees with `svn diff`.
//! * [`diff_as_binary_by_mime`], [`detect_binary_for_add`] and [`detect_encoding`] classify
//!   content; see the `binary` module docs for which one to use when.
//!
//! Diff and merge entry points take a [`svn_types::CancelToken`] and may fail with
//! [`DiffError`]; run them on a blocking thread.
//!
//! All functions operate on bytes; inputs need not be valid UTF-8.

mod affix;
mod binary;
mod control;
mod diff_core;
mod display;
mod lcs;
mod merge;
mod tokens;
mod unified;

pub use binary::{TextEncoding, detect_binary_for_add, detect_encoding, diff_as_binary_by_mime};
pub use control::DiffError;
pub use display::{DisplayOptions, Hunk, HunkLine, LineKind, diff};
pub use merge::{ConflictLabels, ConflictRegion, ConflictStyle, MergeOptions, MergeResult, merge3};
pub use tokens::{DiffOptions, IgnoreSpace, detect_eol, split_lines};
pub use unified::{
    DEFAULT_CONTEXT_SIZE, UnifiedOptions, contains_diffs, index_header, native_eol, unified,
};
