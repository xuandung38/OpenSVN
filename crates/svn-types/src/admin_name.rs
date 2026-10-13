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
    let name = fold(segment);
    name == ".svn" || is_short_name(&name)
}

/// Like [`is_admin_dir_alias`], but also matches `_svn`, the administrative directory name of
/// clients built with `SVN_ASP_DOT_NET_HACK`. Used for names of new folders the user types, which
/// must not be mistaken for an administrative directory by any client.
pub fn is_any_admin_dir_name(segment: &str) -> bool {
    let name = fold(segment);
    name == ".svn" || name == "_svn" || is_short_name(&name)
}

/// NFKC, without file-system ignorable code points, case folded, trailing dots and spaces removed.
fn fold(segment: &str) -> String {
    let folded: String = segment
        .nfkc()
        .filter(|c| !is_fs_ignorable(*c))
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
        .collect();
    folded.trim_end_matches(['.', ' ']).to_owned()
}

/// The NTFS 8.3 short name of `.svn`: `SVN~<digits>` (already folded).
fn is_short_name(name: &str) -> bool {
    name.strip_prefix("svn~")
        .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_admin_name_covers_both_spellings_and_aliases() {
        for n in [
            ".svn",
            ".SVN",
            "_svn",
            "_SvN",
            "_svn.",
            "_svn ",
            ".svn. .",
            "\u{FF3F}svn",
            "_s\u{200D}vn",
            "SVN~1",
            "_ſvn",
        ] {
            assert!(is_any_admin_dir_name(n), "{n:?}");
        }
        for n in ["svn", "_svnx", "x_svn", "__svn", ".svn2"] {
            assert!(!is_any_admin_dir_name(n), "{n:?}");
        }
        assert!(!is_admin_dir_alias("_svn"), "server paths may contain _svn");
    }
}
