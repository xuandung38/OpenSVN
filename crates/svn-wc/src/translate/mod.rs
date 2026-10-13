//! Repository-normal form <-> working-copy form translation, byte-compatible with
//! libsvn_subr `subst.c` and libsvn_wc `translate.c` (svn 1.14.5 and 1.15.0).
//!
//! * [`to_working`]: repo-normal (pristine) bytes -> working file bytes (expand keywords,
//!   convert line endings, always repairing mixed endings like svn does on install).
//! * [`to_repo_normal`]: working bytes -> repo-normal bytes (contract keywords, normalise
//!   line endings; mixed endings fail for `native` unless `force_repair`).
//! * [`special`]: `svn:special` symlink representation (`link <target>`).
//! * [`flags`]: `svn:executable` / `svn:needs-lock` permission helpers.

mod engine;
mod error;
pub mod flags;
mod keywords;
pub mod special;

pub use error::TranslateError;
pub use keywords::{KeywordContext, Keywords};

use engine::{Params, translate};

/// Value of `svn:eol-style` (`svn_subst_eol_style_t`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EolStyle {
    /// Property absent: line endings are never touched.
    #[default]
    None,
    /// `native`: platform line ending in the working copy, LF in the repository.
    Native,
    /// `LF`.
    Lf,
    /// `CR`.
    Cr,
    /// `CRLF`.
    CrLf,
    /// Any other value; svn expands keywords only and refuses to normalise.
    Unknown,
}

impl EolStyle {
    /// Parses a property value (`svn_subst_eol_style_from_value`).
    pub fn from_prop(value: Option<&[u8]>) -> Self {
        match value {
            None => Self::None,
            Some(b"native") => Self::Native,
            Some(b"LF") => Self::Lf,
            Some(b"CR") => Self::Cr,
            Some(b"CRLF") => Self::CrLf,
            Some(_) => Self::Unknown,
        }
    }

    /// Fixed line ending of the style (`None` for `native`, `none` and unknown).
    fn fixed(self) -> Option<&'static [u8]> {
        match self {
            Self::Lf => Some(b"\n"),
            Self::Cr => Some(b"\r"),
            Self::CrLf => Some(b"\r\n"),
            _ => None,
        }
    }
}

/// Line ending of the current platform (`APR_EOL_STR`).
pub const fn platform_eol() -> &'static [u8] {
    if cfg!(windows) { b"\r\n" } else { b"\n" }
}

/// The translation-relevant properties of a node (`svn_wc__get_translate_info`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranslateInfo {
    /// `svn:eol-style`.
    pub eol_style: EolStyle,
    /// Raw `svn:keywords` value; `None` when absent or empty.
    pub keywords_prop: Option<Vec<u8>>,
    /// `svn:special` is set (any value).
    pub special: bool,
}

impl TranslateInfo {
    /// Reads the relevant properties through `get`, which returns a property value by name.
    pub fn from_props<'a>(get: impl Fn(&str) -> Option<&'a [u8]>) -> Self {
        Self {
            eol_style: EolStyle::from_prop(get("svn:eol-style")),
            keywords_prop: get("svn:keywords")
                .filter(|v| !v.is_empty())
                .map(<[u8]>::to_vec),
            special: get("svn:special").is_some(),
        }
    }

    /// `svn_subst_translation_required` with `force_eol_check = TRUE`, as used by libsvn_wc.
    pub fn translation_required(&self) -> bool {
        self.special || self.keywords_prop.is_some() || self.eol_style != EolStyle::None
    }
}

/// Repo-normal -> working bytes using the platform line ending for `native`.
pub fn to_working(
    info: &TranslateInfo,
    input: &[u8],
    ctx: &KeywordContext,
) -> Result<Vec<u8>, TranslateError> {
    to_working_with_native(info, input, ctx, platform_eol())
}

/// Like [`to_working`] with an explicit `native` line ending (used to test other platforms).
pub fn to_working_with_native(
    info: &TranslateInfo,
    input: &[u8],
    ctx: &KeywordContext,
    native_eol: &[u8],
) -> Result<Vec<u8>, TranslateError> {
    if !info.translation_required() {
        return Ok(input.to_vec());
    }
    let keywords = info
        .keywords_prop
        .as_deref()
        .map(|p| Keywords::build(p, ctx));
    let eol = match info.eol_style {
        EolStyle::Native => Some(native_eol),
        other => other.fixed(),
    };
    translate(
        input,
        &Params {
            eol,
            repair: true,
            keywords: keywords.as_ref(),
            expand: true,
        },
    )
}

/// Working bytes -> repo-normal bytes. `force_repair` mirrors `SVN_WC_TRANSLATE_FORCE_EOL_REPAIR`:
/// without it a `native` file with mixed line endings yields [`TranslateError::InconsistentEol`].
pub fn to_repo_normal(
    info: &TranslateInfo,
    input: &[u8],
    force_repair: bool,
) -> Result<Vec<u8>, TranslateError> {
    if !info.translation_required() {
        return Ok(input.to_vec());
    }
    let (eol, repair) = match info.eol_style {
        EolStyle::None => (None, force_repair),
        EolStyle::Native => (Some(&b"\n"[..]), force_repair),
        EolStyle::Unknown => return Err(TranslateError::UnknownEol),
        fixed => (fixed.fixed(), true),
    };
    let keywords = info
        .keywords_prop
        .as_deref()
        .map(Keywords::for_normalization);
    translate(
        input,
        &Params {
            eol,
            repair,
            keywords: keywords.as_ref(),
            expand: false,
        },
    )
}

#[cfg(test)]
mod tests;
