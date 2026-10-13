//! Subversion-compatible text diff, unified output and three-way merge.
//!
//! * [`unified`] and [`merge3`] port the algorithms of Apache Subversion's `libsvn_diff`
//!   (1.15.0, Apache License 2.0) so that their output matches `svn diff` and the conflict
//!   files produced by `svn update` byte for byte.
//! * [`diff`] gives display hunks computed with `imara-diff`; boundaries may differ from svn.
//! * [`is_binary`] and [`detect_encoding`] classify content for the UI.
//!
//! All functions operate on bytes; inputs need not be valid UTF-8.

mod affix;
mod binary;
mod diff_core;
mod display;
mod lcs;
mod merge;
mod tokens;
mod unified;

pub use binary::{TextEncoding, detect_encoding, is_binary};
pub use display::{DisplayOptions, Hunk, HunkLine, LineKind, diff};
pub use merge::{ConflictLabels, ConflictRegion, ConflictStyle, MergeOptions, MergeResult, merge3};
pub use tokens::{DiffOptions, IgnoreSpace, detect_eol, split_lines};
pub use unified::{DEFAULT_CONTEXT_SIZE, UnifiedOptions, contains_diffs, index_header, unified};
