//! Working file content helpers: translation to repository-normal form and digests.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

use md5::{Digest as _, Md5};
use sha1::Sha1;
use svn_types::{RelPath, Result, SvnError};
use svn_wc::WcDb;
use svn_wc::pristine::{Md5Digest, Sha1Digest};
use svn_wc::translate::{ContentTranslator, TranslateInfo, special, translate_stream};

/// Digests and size of a file in repository-normal form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentDigest {
    /// SHA-1 (pristine key).
    pub sha1: Sha1Digest,
    /// MD5 (sent to the server).
    pub md5: Md5Digest,
    /// Size in bytes of the repository-normal text.
    pub size: u64,
}

/// Hashes everything written through it.
struct HashingWriter<W: Write> {
    inner: W,
    sha1: Sha1,
    /// `None` when only the SHA-1 is wanted (comparing with a pristine key).
    md5: Option<Md5>,
    size: u64,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.sha1.update(&buf[..n]);
        if let Some(md5) = &mut self.md5 {
            md5.update(&buf[..n]);
        }
        self.size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Translation-relevant properties of `rel` (the working ones).
pub fn translate_info(db: &WcDb, rel: &RelPath) -> Result<TranslateInfo> {
    let props = db.actual_props(rel)?;
    Ok(TranslateInfo::from_props(|name| {
        props
            .iter()
            .find(|(k, _)| k.as_slice() == name.as_bytes())
            .map(|(_, v)| v.as_slice())
    }))
}

/// Streams the working file at `abs` in repository-normal form into `out` and returns its
/// digests. Mixed line endings under `svn:eol-style=native` fail like they do in svn.
pub fn detranslate_into(
    db: &WcDb,
    rel: &RelPath,
    abs: &Path,
    out: &mut dyn Write,
) -> Result<ContentDigest> {
    let (sha1, md5, size) = detranslate_hashing(db, rel, abs, out, true)?;
    Ok(ContentDigest {
        sha1,
        md5: md5.unwrap_or(Md5Digest([0; 16])),
        size,
    })
}

fn detranslate_hashing(
    db: &WcDb,
    rel: &RelPath,
    abs: &Path,
    out: &mut dyn Write,
    want_md5: bool,
) -> Result<(Sha1Digest, Option<Md5Digest>, u64)> {
    let info = translate_info(db, rel)?;
    let mut hw = HashingWriter {
        inner: out,
        sha1: Sha1::new(),
        md5: want_md5.then(Md5::new),
        size: 0,
    };
    if info.special {
        let bytes = special::read_repo_normal(abs).map_err(SvnError::from)?;
        hw.write_all(&bytes)
            .map_err(|e| SvnError::from_io("write translated text", &e))?;
    } else {
        let translator = ContentTranslator::to_repo_normal(&info, false).map_err(SvnError::from)?;
        let mut file = File::open(abs)
            .map_err(|e| SvnError::from_io(format!("open '{}'", abs.display()), &e))?;
        translate_stream(&mut file, &mut hw, translator).map_err(SvnError::from)?;
    }
    Ok((
        Sha1Digest(hw.sha1.finalize().into()),
        hw.md5.map(|m| Md5Digest(m.finalize().into())),
        hw.size,
    ))
}

/// SHA-1 of the working file in repository-normal form: what is compared with a pristine
/// key, at half the hashing cost of [`detranslate_digest`].
pub fn detranslate_sha1(db: &WcDb, rel: &RelPath, abs: &Path) -> Result<Sha1Digest> {
    Ok(detranslate_hashing(db, rel, abs, &mut io::sink(), false)?.0)
}

/// Digests of the working file in repository-normal form, without keeping the output.
pub fn detranslate_digest(db: &WcDb, rel: &RelPath, abs: &Path) -> Result<ContentDigest> {
    detranslate_into(db, rel, abs, &mut io::sink())
}

/// Writes the repository-normal form of the working file to `dest` (created or truncated, not
/// synced: the caller flushes it, typically together with others) and returns its digests.
pub fn detranslate_to_file(
    db: &WcDb,
    rel: &RelPath,
    abs: &Path,
    dest: &Path,
) -> Result<ContentDigest> {
    let mut file = File::create(dest)
        .map_err(|e| SvnError::from_io(format!("create '{}'", dest.display()), &e))?;
    detranslate_into(db, rel, abs, &mut file)
}

/// Size and modification time (APR microseconds) of `path`, as svn caches them.
pub fn stat_cache(path: &Path) -> Option<(i64, i64)> {
    let md = fs::metadata(path).ok()?;
    let micros = md
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_micros();
    Some((i64::try_from(md.len()).ok()?, i64::try_from(micros).ok()?))
}
