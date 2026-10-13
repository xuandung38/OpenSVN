//! `file_modified_p`: has the working file's text changed relative to its pristine?
//!
//! Port of `svn_wc__internal_file_modified_p` and `compare_and_verify` from
//! `libsvn_wc/questions.c` (identical in svn 1.14.5 and 1.15.0), non-exact mode:
//!
//! 1. A node without a pristine text, that is not a file, or whose status has no pristine, is
//!    reported modified.
//! 2. No regular file on disk: not modified (the text is *missing*, which status reports apart).
//! 3. Fast path: the recorded size and mtime (`translated_size`, `last_mod_time`) both equal the
//!    file's. A recorded size of `NULL` skips the size check; a recorded mtime of `NULL` or `0`
//!    never matches. No file content is read.
//! 4. Without translation a different size means modified without reading anything.
//! 5. Otherwise the working file is converted to repository-normal form (keywords contracted,
//!    line endings normalised with repair) while it is compared with the pristine text.
//!
//! When the compare proves the file unchanged, the caller may repair the stat cache with the size
//! and mtime returned in [`Verdict::unchanged_stat`] (svn does so only when it holds the write
//! lock).
//!
//! Intentional difference: svn's *exact* comparison mode (retranslate the pristine and compare
//! with the raw working file) is not implemented; status never uses it.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use svn_types::{ErrorCode, SvnError};

use crate::pristine::Sha1Digest;
use crate::translate::special::read_repo_normal;
use crate::translate::{ContentTranslator, TranslateInfo};
use crate::workqueue::PristineSource;

/// What the filesystem says about a working file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskFile {
    /// Size in bytes.
    pub size: u64,
    /// Modification time in microseconds since the Unix epoch (APR time).
    pub mtime_micros: i64,
}

/// What wc.db recorded about the node.
#[derive(Debug, Clone)]
pub struct RecordedFile {
    /// Pristine checksum; `None` for a node without pristine (a local addition).
    pub checksum: Option<Sha1Digest>,
    /// `translated_size`.
    pub size: Option<i64>,
    /// `last_mod_time`.
    pub mtime_micros: Option<i64>,
    /// Translation settings from the node's properties; `None` when it has no properties.
    pub translate: Option<TranslateInfo>,
}

/// Result of [`file_modified`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    /// The text differs from the pristine.
    pub modified: bool,
    /// Set when a full compare proved the file unchanged: the stat to record in wc.db so the next
    /// status can use the fast path (svn does this only while holding the write lock).
    pub unchanged_stat: Option<DiskFile>,
}

fn io_err(what: &str, path: &Path, e: &std::io::Error) -> SvnError {
    SvnError::from_io(format!("{what} {}", path.display()), e)
}

/// Decides whether the working file is modified. `disk` is `None` when no regular file exists.
pub fn file_modified(
    path: &Path,
    disk: Option<DiskFile>,
    recorded: &RecordedFile,
    pristine: &dyn PristineSource,
) -> Result<Verdict, SvnError> {
    let unmodified = Verdict { modified: false, unchanged_stat: None };
    let Some(checksum) = recorded.checksum.as_ref() else {
        return Ok(Verdict { modified: true, unchanged_stat: None });
    };
    let Some(disk) = disk else { return Ok(unmodified) };

    // Fast path: size and mtime identical to what was recorded when the file was last verified.
    let size_matches = recorded.size.is_none_or(|s| u64::try_from(s).is_ok_and(|s| s == disk.size));
    let mtime_matches = recorded.mtime_micros.is_some_and(|m| m != 0 && m == disk.mtime_micros);
    if size_matches && mtime_matches {
        return Ok(unmodified);
    }

    let modified = compare_with_pristine(path, disk.size, checksum, recorded.translate.as_ref(), pristine)?;
    Ok(Verdict { modified, unchanged_stat: (!modified).then_some(disk) })
}

/// `compare_and_verify`.
fn compare_with_pristine(
    path: &Path,
    working_size: u64,
    checksum: &Sha1Digest,
    translate: Option<&TranslateInfo>,
    pristine: &dyn PristineSource,
) -> Result<bool, SvnError> {
    let need_translation = translate.is_some_and(TranslateInfo::translation_required);
    if let Some(info) = translate
        && info.eol_style == crate::translate::EolStyle::Unknown
    {
        return Err(SvnError::new(ErrorCode::Corrupt, "unknown svn:eol-style value").with_svn_code(135_001));
    }
    let pristine_size = pristine.text_size(checksum)?;
    // Easy out: without translation a different size means a modified file.
    if !need_translation && working_size != pristine_size {
        return Ok(true);
    }
    let mut pristine_reader = pristine.open_text(checksum)?;

    let info = translate.filter(|_| need_translation);
    if let Some(info) = info
        && info.special
    {
        let text = read_repo_normal(path)?;
        return Ok(!same(&mut text.as_slice(), &mut pristine_reader, None)?);
    }
    let mut working = File::open(path).map_err(|e| io_err("open", path, &e))?;
    let translator = match info {
        Some(info) => Some(ContentTranslator::to_repo_normal(info, true)?),
        None => None,
    };
    Ok(!same(&mut working, &mut pristine_reader, translator)?)
}

/// Compares `working` (translated on the fly) with `pristine` byte for byte. The working file
/// is read in the chunk size svn uses so malformed keyword candidates behave identically.
fn same(
    working: &mut dyn Read,
    pristine: &mut dyn Read,
    mut translator: Option<ContentTranslator>,
) -> Result<bool, SvnError> {
    let chunk = translator.as_ref().map_or(64 * 1024, ContentTranslator::chunk_size);
    let mut buf = vec![0u8; chunk];
    let mut translated = Vec::new();
    let mut expected = vec![0u8; chunk.max(64 * 1024)];
    loop {
        let n = read_full(working, &mut buf)?;
        translated.clear();
        match translator.as_mut() {
            Some(t) => t.push(&buf[..n], &mut translated)?,
            None => translated.extend_from_slice(&buf[..n]),
        }
        if n == 0 {
            if let Some(t) = translator.take() {
                t.finish(&mut translated)?;
            }
        }
        if !matches_prefix(pristine, &translated, &mut expected)? {
            return Ok(false);
        }
        if n == 0 {
            break;
        }
    }
    // The pristine must end exactly here.
    let mut probe = [0u8; 1];
    Ok(read_full(pristine, &mut probe)? == 0)
}

/// Reads `data.len()` bytes from `pristine` and compares them with `data`.
fn matches_prefix(pristine: &mut dyn Read, data: &[u8], scratch: &mut [u8]) -> Result<bool, SvnError> {
    for part in data.chunks(scratch.len()) {
        let got = &mut scratch[..part.len()];
        if read_full(pristine, got)? != part.len() || got != part {
            return Ok(false);
        }
    }
    Ok(true)
}

fn read_full(reader: &mut dyn Read, buf: &mut [u8]) -> Result<usize, SvnError> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(SvnError::from_io("read", &e)),
        }
    }
    Ok(filled)
}
