//! Content-addressed pristine text store (track B).
//!
//! Layout is the libsvn_wc one: `<adm>/pristine/<xx>/<sha1 hex>.svn-base` where `<xx>` is the
//! first two hex digits, and temporary files live in `<adm>/tmp` (same filesystem, so the final
//! rename is atomic). Installing streams the content through SHA-1 and MD5 into a temp file,
//! verifies the expected checksums, fsyncs, then renames into place and makes the file read-only.
//!
//! The database side (the `PRISTINE` table: refcount maintained by triggers, `hydrated` column of
//! format 32) is reached through [`PristineIndex`]; `WcDb` implements it once the schema lands.

use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use md5::Md5;
use sha1::{Digest, Sha1};

/// File suffix of stored pristines.
const SUFFIX: &str = ".svn-base";

/// SHA-1 digest, the key of the pristine store.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sha1Digest(pub [u8; 20]);

/// MD5 digest, stored next to the SHA-1 for compatibility with svn.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Md5Digest(pub [u8; 16]);

fn to_hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn from_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

impl Sha1Digest {
    /// Lower-case hex form.
    pub fn to_hex(&self) -> String {
        to_hex(&self.0)
    }

    /// Parses 40 hex digits (either case).
    pub fn from_hex(s: &str) -> Option<Self> {
        from_hex(s).map(Self)
    }

    /// svn's stored form, `$sha1$<hex>`.
    pub fn to_svn_string(&self) -> String {
        format!("$sha1${}", self.to_hex())
    }

    /// Parses svn's stored form, `$sha1$<hex>`.
    pub fn from_svn_string(s: &str) -> Option<Self> {
        Self::from_hex(s.strip_prefix("$sha1$")?)
    }
}

impl Md5Digest {
    /// Lower-case hex form.
    pub fn to_hex(&self) -> String {
        to_hex(&self.0)
    }

    /// Parses 32 hex digits (either case).
    pub fn from_hex(s: &str) -> Option<Self> {
        from_hex(s).map(Self)
    }

    /// svn's stored form, `$md5 $<hex>` (note the space).
    pub fn to_svn_string(&self) -> String {
        format!("$md5 ${}", self.to_hex())
    }

    /// Parses svn's stored form, `$md5 $<hex>`.
    pub fn from_svn_string(s: &str) -> Option<Self> {
        Self::from_hex(s.strip_prefix("$md5 $")?)
    }
}

impl fmt::Debug for Sha1Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha1:{}", self.to_hex())
    }
}

impl fmt::Debug for Md5Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "md5:{}", self.to_hex())
    }
}

/// One row of the `PRISTINE` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PristineRecord {
    /// Content key.
    pub sha1: Sha1Digest,
    /// MD5 of the same content.
    pub md5: Md5Digest,
    /// Content size in bytes.
    pub size: u64,
    /// `false` when the text was dehydrated (format 32): the row exists, the file does not.
    /// Always `true` for format 31.
    pub hydrated: bool,
}

/// Database side of the pristine store. Implemented over `WcDb`; refcounts are maintained by the
/// schema triggers (plus ACTUAL_NODE checksum columns, which the implementation must count).
pub trait PristineIndex {
    /// Inserts the row if absent; an existing row (and its refcount) is left untouched.
    fn record(&mut self, record: &PristineRecord) -> Result<(), PristineError>;
    /// Looks a row up by SHA-1.
    fn lookup(&self, sha1: &Sha1Digest) -> Result<Option<PristineRecord>, PristineError>;
    /// Rows with refcount 0 that no ACTUAL_NODE checksum column references.
    fn unreferenced(&self) -> Result<Vec<Sha1Digest>, PristineError>;
    /// Deletes a row.
    fn forget(&mut self, sha1: &Sha1Digest) -> Result<(), PristineError>;
}

/// Failure of a pristine store operation.
#[derive(Debug, thiserror::Error)]
pub enum PristineError {
    /// Filesystem failure.
    #[error("{op} '{}': {source}", path.display())]
    Io {
        /// What was attempted.
        op: &'static str,
        /// File involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Content does not match the checksum the caller expected.
    #[error("checksum mismatch: expected {expected}, actual {actual}")]
    ChecksumMismatch {
        /// Expected digest, formatted.
        expected: String,
        /// Computed digest, formatted.
        actual: String,
    },
    /// No row (or no file) for this checksum.
    #[error("pristine text {0:?} is not in the store")]
    NotInstalled(Sha1Digest),
    /// Row exists but the text was dehydrated; it must be fetched from the repository.
    #[error("pristine text {0:?} is not hydrated (fetch it from the repository)")]
    NotHydrated(Sha1Digest),
    /// The database side failed.
    #[error("pristine index: {0}")]
    Index(String),
}

impl PristineError {
    fn io(op: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.to_path_buf();
        move |source| Self::Io { op, path, source }
    }
}

/// Pristine store rooted at a wc administrative directory (`.svn`).
#[derive(Debug)]
pub struct PristineStore<I> {
    adm_dir: PathBuf,
    index: I,
}

impl<I: PristineIndex> PristineStore<I> {
    /// Opens the store of the given `.svn` directory. Directories are created on demand.
    pub fn new(adm_dir: impl Into<PathBuf>, index: I) -> Self {
        Self {
            adm_dir: adm_dir.into(),
            index,
        }
    }

    /// The database index.
    pub fn index(&self) -> &I {
        &self.index
    }

    /// Mutable access to the database index.
    pub fn index_mut(&mut self) -> &mut I {
        &mut self.index
    }

    /// Where the text with this checksum lives (or would live).
    pub fn path_of(&self, sha1: &Sha1Digest) -> PathBuf {
        let hex = sha1.to_hex();
        self.adm_dir
            .join("pristine")
            .join(&hex[..2])
            .join(format!("{hex}{SUFFIX}"))
    }

    /// Streams `reader` into the store and returns the record of what was stored.
    ///
    /// The content is written to a temp file in `<adm>/tmp`, hashed, checked against
    /// `expected_sha1`/`expected_md5` when given, synced, and renamed into place. Content that is
    /// already stored is not rewritten. The row is inserted through the index after the file is in
    /// place, so a crash leaves at worst an unreferenced file, never a row without a file.
    pub fn install(
        &mut self,
        reader: &mut dyn Read,
        expected_sha1: Option<&Sha1Digest>,
        expected_md5: Option<&Md5Digest>,
    ) -> Result<PristineRecord, PristineError> {
        let tmp_dir = self.adm_dir.join("tmp");
        fs::create_dir_all(&tmp_dir).map_err(PristineError::io("create directory", &tmp_dir))?;
        let mut tmp = tempfile::Builder::new()
            .prefix("pristine-")
            .tempfile_in(&tmp_dir)
            .map_err(PristineError::io("create temp file in", &tmp_dir))?;

        let (mut sha, mut md5) = (Sha1::new(), Md5::new());
        let mut size = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(PristineError::io("read stream for", tmp.path()))?;
            if n == 0 {
                break;
            }
            sha.update(&buf[..n]);
            md5.update(&buf[..n]);
            size += n as u64;
            tmp.write_all(&buf[..n])
                .map_err(PristineError::io("write", tmp.path()))?;
        }
        let record = PristineRecord {
            sha1: Sha1Digest(sha.finalize().into()),
            md5: Md5Digest(md5.finalize().into()),
            size,
            hydrated: true,
        };
        if let Some(expected) = expected_sha1.filter(|e| **e != record.sha1) {
            return Err(PristineError::ChecksumMismatch {
                expected: expected.to_svn_string(),
                actual: record.sha1.to_svn_string(),
            });
        }
        if let Some(expected) = expected_md5.filter(|e| **e != record.md5) {
            return Err(PristineError::ChecksumMismatch {
                expected: expected.to_svn_string(),
                actual: record.md5.to_svn_string(),
            });
        }

        let dest = self.path_of(&record.sha1);
        if !dest.exists() {
            tmp.as_file()
                .sync_all()
                .map_err(PristineError::io("sync", tmp.path()))?;
            let parent = dest.parent().unwrap_or(&self.adm_dir);
            fs::create_dir_all(parent).map_err(PristineError::io("create directory", parent))?;
            tmp.persist(&dest).map_err(|e| PristineError::Io {
                op: "rename to",
                path: dest.clone(),
                source: e.error,
            })?;
            // A pristine is immutable, like svn leaves it.
            let mut perms = fs::metadata(&dest)
                .map_err(PristineError::io("stat", &dest))?
                .permissions();
            perms.set_readonly(true);
            fs::set_permissions(&dest, perms)
                .map_err(PristineError::io("set permissions of", &dest))?;
        }
        self.index.record(&record)?;
        Ok(record)
    }

    /// Opens the stored text. Fails with [`PristineError::NotHydrated`] when the row says the
    /// text was dropped, and [`PristineError::NotInstalled`] when row or file is missing.
    pub fn open(&self, sha1: &Sha1Digest) -> Result<File, PristineError> {
        let row = self
            .index
            .lookup(sha1)?
            .ok_or(PristineError::NotInstalled(*sha1))?;
        if !row.hydrated {
            return Err(PristineError::NotHydrated(*sha1));
        }
        let path = self.path_of(sha1);
        File::open(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => PristineError::NotInstalled(*sha1),
            _ => PristineError::Io {
                op: "open",
                path,
                source: e,
            },
        })
    }

    /// Reads and re-hashes the stored text; `true` when it still matches its checksum.
    pub fn verify(&self, sha1: &Sha1Digest) -> Result<bool, PristineError> {
        let mut file = self.open(sha1)?;
        let mut hasher = Sha1::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file
                .read(&mut buf)
                .map_err(PristineError::io("read", &self.path_of(sha1)))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(Sha1Digest(hasher.finalize().into()) == *sha1)
    }

    /// Removes every text the index reports as unreferenced (cleanup). Returns how many were
    /// removed. A missing file is not an error; the row is dropped last, so an interrupted run
    /// is simply repeated.
    pub fn remove_unreferenced(&mut self) -> Result<usize, PristineError> {
        let victims = self.index.unreferenced()?;
        for sha1 in &victims {
            let path = self.path_of(sha1);
            // Unix deletes by directory permission; Windows refuses read-only files.
            #[cfg(windows)]
            if let Ok(meta) = fs::metadata(&path) {
                let mut perms = meta.permissions();
                perms.set_readonly(false);
                let _ = fs::set_permissions(&path, perms);
            }
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(PristineError::Io {
                        op: "remove",
                        path,
                        source: e,
                    });
                }
            }
            self.index.forget(sha1)?;
        }
        Ok(victims.len())
    }
}

#[cfg(test)]
mod tests;
