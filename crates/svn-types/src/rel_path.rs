//! Validated, normalized relative paths.

use std::fmt;

use unicode_normalization::{UnicodeNormalization, is_nfc};

use crate::admin_name::is_admin_dir_alias;
use crate::error::{ErrorCode, Result, SvnError};

/// Relative path with `/` separators in NFC form. The empty path is the root.
///
/// Invariants: no empty segment, no `.` or `..` segment, no NUL. Construct with
/// [`RelPath::new`] for paths produced inside the engine and with
/// [`RelPath::from_server`] for anything that came from a repository or server.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelPath(String);

fn unsafe_path(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::UnsafePath, msg)
}

fn normalize(s: &str) -> String {
    if is_nfc(s) {
        s.to_owned()
    } else {
        s.nfc().collect()
    }
}

/// Structural validation shared by both constructors (applied after NFC).
fn check_structure(s: &str) -> Result<()> {
    if s.is_empty() {
        return Ok(());
    }
    if s.contains('\0') {
        return Err(unsafe_path("path contains NUL"));
    }
    // Never valid in a repository relpath, and the way absolute, drive-relative and UNC paths
    // (`C:\x`, `\\srv\share`) or traversal on Windows get past a `/`-only check.
    if s.contains(['\\', ':']) {
        return Err(unsafe_path(format!("backslash or colon in {s:?}")));
    }
    for seg in s.split('/') {
        match seg {
            "" => return Err(unsafe_path(format!("empty path segment in {s:?}"))),
            "." | ".." => return Err(unsafe_path(format!("relative segment {seg:?} in {s:?}"))),
            _ => {}
        }
    }
    Ok(())
}

/// Whether a segment is a name Windows treats as a device, with or without extension.
fn is_windows_reserved(seg: &str) -> bool {
    let stem = seg.split('.').next().unwrap_or(seg).trim_end_matches(' ');
    let upper = stem.to_uppercase();
    match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        _ => {
            let mut chars = upper.chars();
            let head: String = chars.by_ref().take(3).collect();
            let rest: Vec<char> = chars.collect();
            (head == "COM" || head == "LPT")
                && rest.len() == 1
                && matches!(rest[0], '0'..='9' | '\u{00B9}' | '\u{00B2}' | '\u{00B3}')
        }
    }
}

/// Segment checks that apply only to untrusted names.
fn check_untrusted_segment(seg: &str, whole: &str) -> Result<()> {
    if seg.chars().any(|c| c.is_control()) {
        return Err(unsafe_path(format!("control character in {whole:?}")));
    }
    if seg.contains('\\') {
        return Err(unsafe_path(format!("backslash in {whole:?}")));
    }
    if seg.contains(':') {
        return Err(unsafe_path(format!(
            "colon (stream separator) in {whole:?}"
        )));
    }
    if seg.contains(['<', '>', '"', '|', '?', '*']) {
        return Err(unsafe_path(format!(
            "character invalid on Windows in {whole:?}"
        )));
    }
    if is_admin_dir_alias(seg) {
        return Err(unsafe_path(format!(
            "administrative directory name in {whole:?}"
        )));
    }
    if seg.ends_with('.') || seg.ends_with(' ') {
        return Err(unsafe_path(format!("trailing dot or space in {whole:?}")));
    }
    if is_windows_reserved(seg) {
        return Err(unsafe_path(format!("reserved device name in {whole:?}")));
    }
    Ok(())
}

impl RelPath {
    /// The working copy / repository root (empty path).
    pub fn root() -> Self {
        Self(String::new())
    }

    /// Builds a path from engine-internal input; the empty string yields the root.
    /// Rejects empty/`.`/`..` segments, NUL, `\`, `:` (so no absolute, drive or UNC form), but
    /// not the stricter server-name rules of [`RelPath::from_server`].
    pub fn new(s: &str) -> Result<Self> {
        let n = normalize(s);
        check_structure(&n)?;
        Ok(Self(n))
    }

    /// Builds a path from untrusted server or repository text.
    ///
    /// Rejects, on every platform: empty, `.` and `..` segments, leading/trailing/double `/`,
    /// `\`, `:` (NTFS alternate data streams), control characters including NUL, characters
    /// Windows cannot store (`<>"|?*`), any name that aliases `.svn` (Unicode case folding on the
    /// NFKC form, ignorable code points, and the NTFS short name `SVN~<n>`), Windows device
    /// names (`CON`, `PRN`, `AUX`, `NUL`, `COM0-9`, `LPT0-9`, with or without extension) and
    /// segments ending in a dot or space.
    /// The result is NFC-normalized. The empty string is rejected: server paths name a node.
    pub fn from_server(s: &str) -> Result<Self> {
        if s.is_empty() {
            return Err(unsafe_path("empty server path"));
        }
        let n = normalize(s);
        check_structure(&n)?;
        for seg in n.split('/') {
            check_untrusted_segment(seg, &n)?;
        }
        Ok(Self(n))
    }

    /// Path text with `/` separators.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is the root.
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterates segments (none for the root).
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|s| !s.is_empty())
    }

    /// Last segment, `None` for the root.
    pub fn file_name(&self) -> Option<&str> {
        self.components().last()
    }

    /// Parent path, `None` for the root.
    pub fn parent(&self) -> Option<RelPath> {
        if self.is_root() {
            return None;
        }
        Some(Self(
            self.0
                .rsplit_once('/')
                .map(|(p, _)| p)
                .unwrap_or("")
                .to_owned(),
        ))
    }

    /// Appends one segment, validating it like [`RelPath::new`].
    pub fn child(&self, name: &str) -> Result<RelPath> {
        if name.contains('/') {
            return Err(unsafe_path(format!("segment contains '/': {name:?}")));
        }
        if self.is_root() {
            RelPath::new(name)
        } else {
            RelPath::new(&format!("{}/{}", self.0, name))
        }
    }

    /// True when `self` equals `base` or lies beneath it.
    pub fn starts_with(&self, base: &RelPath) -> bool {
        base.is_root()
            || self.0 == base.0
            || (self.0.starts_with(&base.0) && self.0.as_bytes().get(base.0.len()) == Some(&b'/'))
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
