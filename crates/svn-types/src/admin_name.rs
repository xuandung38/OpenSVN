//! Recognition of names that address the `.svn` administrative directory.

use unicode_normalization::UnicodeNormalization;

/// Code points that some file systems ignore when resolving names (HFS+ style), e.g. zero-width
/// joiners and bidi controls. A name containing them can alias a plain one.
fn is_fs_ignorable(c: char) -> bool {
    matches!(c, '\u{200C}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{206A}'..='\u{206F}' | '\u{FEFF}')
}

/// True when `segment` can resolve to the administrative directory on some supported file system.
///
/// The name is normalized to NFKC (fullwidth and compatibility forms), stripped of file-system
/// ignorable code points, folded with full Unicode upper-then-lower casing (so `ſ` becomes `s`),
/// and stripped of trailing dots and spaces (ignored by Windows). It then matches `.svn` or an
/// NTFS 8.3 short name of it: `SVN~<digits>` (any case).
pub(crate) fn is_admin_dir_alias(segment: &str) -> bool {
    let folded: String = segment
        .nfkc()
        .filter(|c| !is_fs_ignorable(*c))
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
        .collect();
    let name = folded.trim_end_matches(['.', ' ']);
    name == ".svn"
        || name
            .strip_prefix("svn~")
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}
