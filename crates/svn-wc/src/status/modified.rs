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
//!    line endings normalised with repair) while it is compared with the pristine text. When the
//!    pristine text is not hydrated (format 32 with `store_pristine = false`) the SHA-1 of the
//!    repository-normal text is compared with the recorded checksum instead, as svn 1.15 does.
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
    let unmodified = Verdict {
        modified: false,
        unchanged_stat: None,
    };
    let Some(checksum) = recorded.checksum.as_ref() else {
        return Ok(Verdict {
            modified: true,
            unchanged_stat: None,
        });
    };
    let Some(disk) = disk else {
        return Ok(unmodified);
    };

    // Fast path: size and mtime identical to what was recorded when the file was last verified.
    let size_matches = recorded
        .size
        .is_none_or(|s| u64::try_from(s).is_ok_and(|s| s == disk.size));
    let mtime_matches = recorded
        .mtime_micros
        .is_some_and(|m| m != 0 && m == disk.mtime_micros);
    if size_matches && mtime_matches {
        return Ok(unmodified);
    }

    let modified = compare_with_pristine(
        path,
        disk.size,
        checksum,
        recorded.translate.as_ref(),
        pristine,
    )?;
    Ok(Verdict {
        modified,
        unchanged_stat: (!modified).then_some(disk),
    })
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
        return Err(
            SvnError::new(ErrorCode::Corrupt, "unknown svn:eol-style value").with_svn_code(135_001),
        );
    }
    let pristine_size = pristine.text_size(checksum)?;
    // Easy out: without translation a different size means a modified file.
    if !need_translation && working_size != pristine_size {
        return Ok(true);
    }
    // A dehydrated pristine (format 32, `store_pristine = false`) has no text on disk; svn 1.15
    // then compares the checksum of the repository-normal working text with the recorded one.
    let mut pristine_reader = match pristine.open_text(checksum) {
        Ok(r) => Some(r),
        Err(crate::pristine::PristineError::NotHydrated(_)) => None,
        Err(e) => return Err(e.into()),
    };

    let info = translate.filter(|_| need_translation);
    if let Some(info) = info
        && info.special
    {
        let text = read_repo_normal(path)?;
        return Ok(!match pristine_reader.as_mut() {
            Some(p) => same(&mut text.as_slice(), p, None)?,
            None => digest_matches(&mut text.as_slice(), None, checksum)?,
        });
    }
    let mut working = File::open(path).map_err(|e| io_err("open", path, &e))?;
    let translator = match info {
        Some(info) => Some(ContentTranslator::to_repo_normal(info, true)?),
        None => None,
    };
    Ok(!match pristine_reader.as_mut() {
        Some(p) => same(&mut working, p, translator)?,
        None => digest_matches(&mut working, translator, checksum)?,
    })
}

/// Feeds `working`, translated on the fly, to `sink` in the chunk size svn uses (so malformed
/// keyword candidates behave identically). Stops early when `sink` returns `false`; returns
/// whether the whole input was consumed.
fn drive(
    working: &mut dyn Read,
    mut translator: Option<ContentTranslator>,
    mut sink: impl FnMut(&[u8]) -> Result<bool, SvnError>,
) -> Result<bool, SvnError> {
    let chunk = translator
        .as_ref()
        .map_or(64 * 1024, ContentTranslator::chunk_size);
    let mut buf = vec![0u8; chunk];
    let mut translated = Vec::new();
    loop {
        let n = read_full(working, &mut buf)?;
        translated.clear();
        match translator.as_mut() {
            Some(t) => t.push(&buf[..n], &mut translated)?,
            None => translated.extend_from_slice(&buf[..n]),
        }
        if n == 0
            && let Some(t) = translator.take()
        {
            t.finish(&mut translated)?;
        }
        if !sink(&translated)? {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

/// SHA-1 of the translated working text equals `expected`.
fn digest_matches(
    working: &mut dyn Read,
    translator: Option<ContentTranslator>,
    expected: &Sha1Digest,
) -> Result<bool, SvnError> {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    drive(working, translator, |data| {
        hasher.update(data);
        Ok(true)
    })?;
    Ok(hasher.finalize().as_slice() == expected.0.as_slice())
}

/// Compares `working` (translated on the fly) with `pristine` byte for byte.
fn same(
    working: &mut dyn Read,
    pristine: &mut dyn Read,
    translator: Option<ContentTranslator>,
) -> Result<bool, SvnError> {
    let mut expected = vec![0u8; 64 * 1024];
    if !drive(working, translator, |data| {
        matches_prefix(pristine, data, &mut expected)
    })? {
        return Ok(false);
    }
    // The pristine must end exactly here.
    let mut probe = [0u8; 1];
    Ok(read_full(pristine, &mut probe)? == 0)
}

/// Reads `data.len()` bytes from `pristine` and compares them with `data`.
fn matches_prefix(
    pristine: &mut dyn Read,
    data: &[u8],
    scratch: &mut [u8],
) -> Result<bool, SvnError> {
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

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::pristine::PristineError;
    use sha1::{Digest, Sha1};

    /// A pristine store whose texts are all dehydrated (format 32, `store_pristine = false`).
    struct Dehydrated(u64);

    impl PristineSource for Dehydrated {
        fn open_text(&self, sha1: &Sha1Digest) -> Result<Box<dyn Read>, PristineError> {
            Err(PristineError::NotHydrated(*sha1))
        }
        fn text_size(&self, _: &Sha1Digest) -> Result<u64, PristineError> {
            Ok(self.0)
        }
    }

    fn digest(data: &[u8]) -> Sha1Digest {
        Sha1Digest(Sha1::digest(data).into())
    }

    fn check(working: &[u8], pristine: &[u8], translate: Option<TranslateInfo>) -> bool {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("f");
        std::fs::write(&path, working).expect("write");
        let recorded = RecordedFile {
            checksum: Some(digest(pristine)),
            size: None,
            mtime_micros: None,
            translate,
        };
        let disk = DiskFile {
            size: working.len() as u64,
            mtime_micros: 1,
        };
        let pristine = Dehydrated(pristine.len() as u64);
        file_modified(&path, Some(disk), &recorded, &pristine)
            .expect("no pristine text must not fail")
            .modified
    }

    #[test]
    fn dehydrated_pristine_is_compared_by_checksum() {
        assert!(!check(b"same\n", b"same\n", None));
        assert!(check(b"diff\n", b"same\n", None)); // same size, other content
        assert!(check(b"longer\n", b"same\n", None)); // size easy-out
        let crlf = TranslateInfo::from_props(|n| (n == "svn:eol-style").then_some(&b"CRLF"[..]));
        assert!(!check(b"a\nb\n", b"a\r\nb\r\n", Some(crlf.clone()))); // eol repaired
        assert!(check(b"a\nc\n", b"a\r\nb\r\n", Some(crlf)));
        let kw = TranslateInfo::from_props(|n| (n == "svn:keywords").then_some(&b"Rev"[..]));
        assert!(!check(b"$Rev: 12 $\n", b"$Rev$\n", Some(kw)));
    }
}
