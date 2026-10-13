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

use std::borrow::Cow;
use std::io::{Read, Write};

pub use error::TranslateError;
pub use keywords::{KeywordContext, Keywords, UtcOffsetAt, utc_offset_local, utc_offset_zero};

use engine::Params;

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

    /// `svn_subst_translation_required` with `force_eol_check = TRUE`, as used by libsvn_wc:
    /// `svn:special`, a keyword set that enables at least one keyword, or any eol style.
    /// (`svn:keywords=unknownword` enables nothing, so it does not count.)
    ///
    /// Note that a `svn:special` file is reported as requiring translation but is never run
    /// through the eol/keyword translator; handle it with [`special`].
    pub fn translation_required(&self) -> bool {
        self.special
            || self.eol_style != EolStyle::None
            || self
                .keywords_prop
                .as_deref()
                .is_some_and(|p| !Keywords::for_normalization(p).is_empty())
    }
}

/// Chunk size svn feeds to the translator when expanding (checkout/install copies the
/// pristine with `SVN_STREAM_CHUNK_SIZE`). Verified against svn 1.15.0.
pub const EXPAND_CHUNK_SIZE: usize = 102_400;

/// Chunk size svn uses when reading a working file to normalise it (commit, status
/// comparison; `SVN__STREAM_CHUNK_SIZE`). Verified against svn 1.15.0. It matters only for
/// malformed keyword candidates that straddle a chunk boundary, but byte parity requires it.
pub const NORMALIZE_CHUNK_SIZE: usize = 16_384;

/// Incremental translation of one file, in either direction.
///
/// Feed chunks of [`ContentTranslator::chunk_size`] bytes (the last one may be shorter) to
/// [`push`](Self::push), then call [`finish`](Self::finish). Memory use is bounded by the chunk
/// size, so multi-gigabyte files with `svn:eol-style` can be streamed.
pub struct ContentTranslator {
    /// `None`: no translation applies, bytes pass through.
    inner: Option<engine::Translator>,
    chunk_size: usize,
}

impl ContentTranslator {
    /// Repo-normal -> working form using the platform line ending for `native`.
    pub fn to_working(info: &TranslateInfo, ctx: &KeywordContext, tz: UtcOffsetAt<'_>) -> Self {
        Self::to_working_with_native(info, ctx, tz, platform_eol())
    }

    /// Like [`to_working`](Self::to_working) with an explicit `native` line ending.
    ///
    /// Always repairs inconsistent line endings, like svn does when installing a file.
    /// A `svn:special` file is never translated (svn bypasses eol and keyword handling for it).
    pub fn to_working_with_native(
        info: &TranslateInfo,
        ctx: &KeywordContext,
        tz: UtcOffsetAt<'_>,
        native_eol: &'static [u8],
    ) -> Self {
        let chunk_size = EXPAND_CHUNK_SIZE;
        if info.special || !info.translation_required() {
            return Self {
                inner: None,
                chunk_size,
            };
        }
        let keywords = info
            .keywords_prop
            .as_deref()
            .map(|p| Keywords::build(p, ctx, tz));
        let eol = match info.eol_style {
            EolStyle::Native => Some(native_eol),
            other => other.fixed(),
        };
        let inner = engine::Translator::new(Params {
            eol,
            repair: true,
            keywords,
            expand: true,
        });
        Self {
            inner: Some(inner),
            chunk_size,
        }
    }

    /// Working -> repo-normal form. `force_repair` mirrors `SVN_WC_TRANSLATE_FORCE_EOL_REPAIR`:
    /// without it a `native` file with mixed line endings yields
    /// [`TranslateError::InconsistentEol`]. Fixed styles always repair. An unknown
    /// `svn:eol-style` fails with [`TranslateError::UnknownEol`]. A `svn:special` file is never
    /// translated here; use [`special::read_repo_normal`].
    pub fn to_repo_normal(
        info: &TranslateInfo,
        force_repair: bool,
    ) -> Result<Self, TranslateError> {
        let chunk_size = NORMALIZE_CHUNK_SIZE;
        if info.special || !info.translation_required() {
            return Ok(Self {
                inner: None,
                chunk_size,
            });
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
        let inner = engine::Translator::new(Params {
            eol,
            repair,
            keywords,
            expand: false,
        });
        Ok(Self {
            inner: Some(inner),
            chunk_size,
        })
    }

    /// Chunk size that reproduces svn's chunking for this direction.
    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// `true` when bytes pass through unchanged.
    pub fn is_identity(&self) -> bool {
        self.inner.is_none()
    }

    /// Translates one chunk, appending the result to `out`.
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<u8>) -> Result<(), TranslateError> {
        match &mut self.inner {
            Some(t) => t.push(chunk, out),
            None => {
                out.extend_from_slice(chunk);
                Ok(())
            }
        }
    }

    /// Flushes the pending state; no more chunks may follow.
    pub fn finish(self, out: &mut Vec<u8>) -> Result<(), TranslateError> {
        match self.inner {
            Some(t) => t.finish(out),
            None => Ok(()),
        }
    }
}

/// Reads up to `buf.len()` bytes, looping over short reads (`svn_stream_read_full`).
fn read_full(reader: &mut dyn Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Streams `reader` through `translator` into `writer` with svn's chunking.
pub fn translate_stream(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    mut translator: ContentTranslator,
) -> Result<(), TranslateError> {
    let io = |op: &'static str| move |source| TranslateError::Stream { op, source };
    let mut buf = vec![0u8; translator.chunk_size()];
    let mut out = Vec::new();
    loop {
        let n = read_full(reader, &mut buf).map_err(io("read"))?;
        if n == 0 {
            break;
        }
        translator.push(&buf[..n], &mut out)?;
        writer.write_all(&out).map_err(io("write"))?;
        out.clear();
    }
    translator.finish(&mut out)?;
    writer.write_all(&out).map_err(io("write"))
}

/// Runs a whole slice through `translator` chunk by chunk.
fn translate_slice<'a>(
    input: &'a [u8],
    mut translator: ContentTranslator,
) -> Result<Cow<'a, [u8]>, TranslateError> {
    if translator.is_identity() {
        return Ok(Cow::Borrowed(input));
    }
    let mut out = Vec::with_capacity(input.len() + input.len() / 16);
    for chunk in input.chunks(translator.chunk_size()) {
        translator.push(chunk, &mut out)?;
    }
    translator.finish(&mut out)?;
    Ok(Cow::Owned(out))
}

/// Repo-normal -> working bytes using the platform line ending for `native`. Borrowed when
/// no translation applies (including `svn:special` files, which svn never translates).
pub fn to_working<'a>(
    info: &TranslateInfo,
    input: &'a [u8],
    ctx: &KeywordContext,
    tz: UtcOffsetAt<'_>,
) -> Result<Cow<'a, [u8]>, TranslateError> {
    translate_slice(input, ContentTranslator::to_working(info, ctx, tz))
}

/// Like [`to_working`] with an explicit `native` line ending (used to test other platforms).
pub fn to_working_with_native<'a>(
    info: &TranslateInfo,
    input: &'a [u8],
    ctx: &KeywordContext,
    tz: UtcOffsetAt<'_>,
    native_eol: &'static [u8],
) -> Result<Cow<'a, [u8]>, TranslateError> {
    translate_slice(
        input,
        ContentTranslator::to_working_with_native(info, ctx, tz, native_eol),
    )
}

/// Working bytes -> repo-normal bytes; see [`ContentTranslator::to_repo_normal`].
pub fn to_repo_normal<'a>(
    info: &TranslateInfo,
    input: &'a [u8],
    force_repair: bool,
) -> Result<Cow<'a, [u8]>, TranslateError> {
    translate_slice(
        input,
        ContentTranslator::to_repo_normal(info, force_repair)?,
    )
}

/// Fails with [`TranslateError::InconsistentEol`] when `input` mixes line-ending styles. This is
/// the check `svn propset svn:eol-style` performs (any style, including fixed ones).
pub fn check_eol_consistency(input: &[u8]) -> Result<(), TranslateError> {
    let mut t = engine::Translator::new(Params {
        eol: Some(b"\n"),
        repair: false,
        keywords: None,
        expand: false,
    });
    let mut sink = Vec::new();
    for chunk in input.chunks(NORMALIZE_CHUNK_SIZE) {
        t.push(chunk, &mut sink)?;
        sink.clear();
    }
    t.finish(&mut sink)
}

#[cfg(test)]
mod tests;
