//! Ignore patterns for unversioned items: svn's default `global-ignores`, `svn:ignore` of the
//! containing directory and `svn:global-ignores` of the directory and its ancestors
//! (`collect_ignore_patterns` in `libsvn_wc/status.c`), matched against the item's base name with
//! `apr_fnmatch` semantics (`svn_wc_match_ignore_list`).

use crate::conflicts::markers::glob_match;

/// svn's built-in `global-ignores` (`SVN_CONFIG_DEFAULT_GLOBAL_IGNORES`) used when the runtime
/// configuration does not set the option.
pub const DEFAULT_GLOBAL_IGNORES: &[&str] = &[
    "*.o",
    "*.lo",
    "*.la",
    "*.al",
    ".libs",
    "*.so",
    "*.so.[0-9]*",
    "*.a",
    "*.pyc",
    "*.pyo",
    "__pycache__",
    "*.rej",
    "*~",
    "#*#",
    ".#*",
    ".*.swp",
    ".DS_Store",
    "[Tt]humbs.db",
];

/// Splits a property value into patterns on CR and LF, dropping empty lines
/// (`svn_cstring_split_append(..., "\n\r", FALSE)`).
pub(crate) fn split_patterns(value: &[u8]) -> impl Iterator<Item = String> + '_ {
    value
        .split(|b| *b == b'\n' || *b == b'\r')
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
}

/// `svn_wc_match_ignore_list`.
pub(crate) fn is_ignored(name: &str, patterns: &[String]) -> bool {
    patterns
        .iter()
        .any(|p| glob_match(p.as_bytes(), name.as_bytes()))
}
