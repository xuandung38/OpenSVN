//! Content-addressed pristine text store (track B).
//!
//! Layout is the libsvn_wc one: `<adm>/pristine/<xx>/<sha1 hex>.svn-base` where `<xx>` is the
//! first two hex digits, and temporary files live in `<adm>/tmp` (same filesystem, so the final
//! rename is atomic). Installing streams the content through SHA-1 and MD5 into a temp file,
//! verifies the expected checksums, fsyncs, renames into place (replacing an orphan file) and
//! makes the file read-only.
//!
//! The database side (the `PRISTINE` table: refcount maintained by triggers, `hydrated` column of
//! format 32) is reached through [`PristineIndex`]; `WcDb` implements it once the schema lands.
//!
//! # Concurrency and ordering
//! [`PristineStore::install`] and [`PristineStore::remove_unreferenced`] must run under the WC
//! write lock, and the caller must record the NODES row that references an installed text in the
//! same database transaction as [`PristineIndex::record`] (the 07A facade owns that transaction).
//! The *row* is authoritative, like in svn: install decides by the row, never by the file; the
//! GC deletes the row first (only if still unreferenced) and the file second. A crash between
//! the two steps leaves an orphan file, which the next install of the same text overwrites.
//!
//! # Format 32
//! A store opened with [`PristineMode::ReadOnly`] (format 32 working copies) can read and verify
//! but refuses every write with [`PristineError::ReadOnlyFormat`]. Re-hydrating a dehydrated
//! text is out of the MVP: `record` never flips `hydrated` from false to true.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use md5::Md5;
use sha1::{Digest, Sha1};
use svn_types::{ErrorCode, SvnError, rename_replacing};

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
/// schema triggers.
///
/// Every method runs in the caller's write transaction under the WC write lock.
pub trait PristineIndex {
    /// Inserts the row if absent; an existing row (and its refcount) is left untouched. It never
    /// flips `hydrated` from false to true (re-hydration is not supported in the MVP).
    fn record(&mut self, record: &PristineRecord) -> Result<(), PristineError>;
    /// Looks a row up by SHA-1.
    fn lookup(&self, sha1: &Sha1Digest) -> Result<Option<PristineRecord>, PristineError>;
    /// Candidates for cleanup: rows with refcount 0. A snapshot only; deletion must go through
    /// [`forget_if_unreferenced`](Self::forget_if_unreferenced).
    fn unreferenced(&self) -> Result<Vec<Sha1Digest>, PristineError>;
    /// Deletes the row **iff** it is still unreferenced, atomically (one transaction:
    /// `DELETE ... WHERE checksum = ? AND refcount = 0` plus the ACTUAL_NODE check below), and
    /// reports whether a row was deleted. Only then may the caller remove the file.
    ///
    /// Intentionally stricter than svn 1.14/1.15, whose cleanup looks at `refcount = 0` only:
    /// implementations must additionally keep rows whose checksum is still named by an
    /// ACTUAL_NODE `older/left/right_checksum` column (legacy columns without triggers). That can
    /// only retain more texts than svn would, never fewer; do not add other semantics.
    fn forget_if_unreferenced(&mut self, sha1: &Sha1Digest) -> Result<bool, PristineError>;
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
    /// Row exists but the text was dehydrated; it must be fetched from the repository. Converts
    /// to [`ErrorCode::PristineNotHydrated`], which callers use to decide to fetch it through RA.
    #[error("pristine text {0:?} is not hydrated (fetch it from the repository)")]
    NotHydrated(Sha1Digest),
    /// The working copy format only allows reading pristines (format 32).
    #[error("working copy format 32 is not supported for writing in this version")]
    ReadOnlyFormat,
    /// The row exists but its text was dehydrated; re-hydration is not supported in the MVP.
    #[error("pristine text {0:?} is dehydrated and cannot be re-hydrated in this version")]
    RehydrateUnsupported(Sha1Digest),
    /// A shared helper failed (for example `rename_replacing`); the classification of the
    /// underlying I/O error (disk full, permission denied, file in use) is kept.
    #[error(transparent)]
    Svn(#[from] SvnError),
    /// The database side failed.
    #[error("pristine index: {0}")]
    Index(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl From<PristineError> for SvnError {
    fn from(err: PristineError) -> Self {
        match err {
            // SVN_ERR_CHECKSUM_MISMATCH
            PristineError::ChecksumMismatch { .. } => {
                SvnError::new(ErrorCode::Corrupt, err.to_string()).with_svn_code(200_014)
            }
            // SVN_ERR_WC_CORRUPT_TEXT_BASE
            PristineError::NotInstalled(_) => {
                SvnError::new(ErrorCode::Corrupt, err.to_string()).with_svn_code(155_017)
            }
            PristineError::NotHydrated(_) => {
                SvnError::new(ErrorCode::PristineNotHydrated, err.to_string())
            }
            PristineError::ReadOnlyFormat | PristineError::RehydrateUnsupported(_) => {
                SvnError::new(ErrorCode::WcFormatUnsupported, err.to_string())
            }
            PristineError::Io {
                op,
                ref path,
                ref source,
            } => SvnError::from_io(format!("{op} '{}'", path.display()), source),
            PristineError::Svn(e) => e,
            PristineError::Index(_) => SvnError::new(ErrorCode::Internal, err.to_string()),
        }
    }
}

impl PristineError {
    /// Wraps a message as an index failure.
    pub fn index(msg: impl Into<String>) -> Self {
        Self::Index(msg.into().into())
    }

    fn io(op: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.to_path_buf();
        move |source| Self::Io { op, path, source }
    }
}

/// What [`PristineStore::place`] did with the file it was handed.
#[derive(Debug)]
pub enum Placement {
    /// The file was moved into the store.
    Placed,
    /// The store already held the text; the file is handed back untouched. A caller that
    /// keeps it until its transaction commits can restore the stored text from it, should
    /// another client remove that text in between (see
    /// [`crate::db::WcTxn::record_pristine_or_restore`]). Dropping it removes the file.
    AlreadyStored(tempfile::TempPath),
}

impl Placement {
    /// True when the file was moved into the store.
    pub fn is_placed(&self) -> bool {
        matches!(self, Self::Placed)
    }
}

/// Whether the store may be modified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PristineMode {
    /// Format 31: install and cleanup allowed.
    Writable,
    /// Format 32: reading only; writes fail with [`PristineError::ReadOnlyFormat`].
    ReadOnly,
}

/// Pristine store rooted at a wc administrative directory (`.svn`).
#[derive(Debug)]
pub struct PristineStore<I> {
    adm_dir: PathBuf,
    mode: PristineMode,
    index: I,
}

impl<I: PristineIndex> PristineStore<I> {
    /// Opens the store of the given `.svn` directory. The facade passes
    /// [`PristineMode::ReadOnly`] for format 32. Directories are created on demand.
    pub fn open(adm_dir: impl Into<PathBuf>, mode: PristineMode, index: I) -> Self {
        Self {
            adm_dir: adm_dir.into(),
            mode,
            index,
        }
    }

    fn require_writable(&self) -> Result<(), PristineError> {
        match self.mode {
            PristineMode::Writable => Ok(()),
            PristineMode::ReadOnly => Err(PristineError::ReadOnlyFormat),
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
        text_path(&self.adm_dir, sha1)
    }

    /// Streams `reader` into the store and returns the record of what is stored.
    ///
    /// The content is written to a temp file in `<adm>/tmp`, hashed, and checked against
    /// `expected_sha1`/`expected_md5` when given. If the index already has a hydrated row for the
    /// checksum, the stored file is kept (it was verified when installed) and the temp file is
    /// dropped. Otherwise the temp file is synced and renamed into place, replacing any orphan
    /// file without a row, the directory entry is synced, and the row is recorded last, so a
    /// crash leaves at worst an orphan file, never a row without a file.
    ///
    /// Requires the WC write lock; see the module documentation.
    pub fn install(
        &mut self,
        reader: &mut dyn Read,
        expected_sha1: Option<&Sha1Digest>,
        expected_md5: Option<&Md5Digest>,
    ) -> Result<PristineRecord, PristineError> {
        self.require_writable()?;
        let tmp_dir = self.adm_dir.join("tmp");
        fs::create_dir_all(&tmp_dir).map_err(PristineError::io("create directory", &tmp_dir))?;
        let (tmp, record) = write_temp(&tmp_dir, reader)?;
        check_expected(&record, expected_sha1, expected_md5)?;

        match self.index.lookup(&record.sha1)? {
            // The row is authoritative: the stored file stays, the temp file is dropped.
            Some(row) if row.hydrated => return Ok(row),
            Some(_) => return Err(PristineError::RehydrateUnsupported(record.sha1)),
            None => {}
        }

        let dest = self.path_of(&record.sha1);
        place(tmp, &dest, &self.adm_dir)?;
        sync_dir(dest.parent().unwrap_or(&self.adm_dir))?;
        self.index.record(&record)?;
        Ok(record)
    }

    /// Moves an already hashed text file into the store **without recording its row**, and
    /// returns whether it did, or whether the store already held the text (the file is then
    /// handed back, see [`Placement::AlreadyStored`]).
    ///
    /// For callers that computed `record` while writing `file` (the update editor hashes the
    /// delta result as it applies it), so the text is neither copied nor hashed again. The caller
    /// must record the row ([`crate::db::WcTxn::record_pristine`]) in the transaction that
    /// references the text, and, when `sync` is false, run [`crate::durable::sync_filesystem`]
    /// on the administrative directory before that transaction commits. A crash before the
    /// commit leaves an orphan file, which the next install of the same text overwrites.
    ///
    /// `sync` = true syncs the file and its directory entry here (the [`install`](Self::install)
    /// behaviour); pass false only when [`crate::durable::batched_sync`] allows a barrier and one
    /// follows.
    ///
    /// `uncommitted` holds the texts this caller placed whose rows are not committed yet. Such a
    /// text is never replaced: an earlier barrier covers the stored file and a transaction about
    /// to commit relies on it, while a new inode renamed over it would be covered only by a later
    /// barrier. The new file is dropped instead (the content is the same, by its checksum). An
    /// orphan left by an earlier, interrupted run (no row, not in the set) is replaced, since it
    /// may be partial. A text with a committed row is never touched either.
    ///
    /// A committed row may be unreferenced (refcount 0): `svn cleanup` in another process can
    /// then remove row and file before the caller's transaction references the text. The
    /// caller records the row with [`crate::db::WcTxn::record_pristine_or_restore`], which
    /// checks for the file inside that transaction.
    ///
    /// Requires the WC write lock; see the module documentation.
    pub fn place(
        &mut self,
        file: tempfile::TempPath,
        record: &PristineRecord,
        sync: bool,
        uncommitted: &mut HashSet<Sha1Digest>,
    ) -> Result<Placement, PristineError> {
        self.require_writable()?;
        match self.index.lookup(&record.sha1)? {
            Some(row) if row.hydrated => return Ok(Placement::AlreadyStored(file)),
            Some(_) => return Err(PristineError::RehydrateUnsupported(record.sha1)),
            None => {}
        }
        if uncommitted.contains(&record.sha1) && self.path_of(&record.sha1).is_file() {
            return Ok(Placement::AlreadyStored(file));
        }
        if sync {
            fs::OpenOptions::new()
                .write(true)
                .open(&file)
                .and_then(|f| f.sync_all())
                .map_err(PristineError::io("sync", &file))?;
        }
        let dest = self.path_of(&record.sha1);
        let parent = dest.parent().unwrap_or(&self.adm_dir);
        if !parent.is_dir() {
            fs::create_dir_all(parent).map_err(PristineError::io("create directory", parent))?;
        }
        #[cfg(feature = "failpoints")]
        fail::fail_point!("pristine.install.before_rename", |_| Err(
            PristineError::index("failpoint pristine.install.before_rename")
        ));
        rename_replacing(&file, &dest)?;
        // The rename consumed the temp file; do not let the guard remove the stored text.
        let _ = file.keep();
        let mut perms = fs::metadata(&dest)
            .map_err(PristineError::io("stat", &dest))?
            .permissions();
        perms.set_readonly(true);
        fs::set_permissions(&dest, perms)
            .map_err(PristineError::io("set permissions of", &dest))?;
        if sync {
            sync_dir(parent)?;
        }
        uncommitted.insert(record.sha1);
        Ok(Placement::Placed)
    }

    /// A writer that puts many texts into this store without recording their rows, for a
    /// caller that records all of them in one database transaction (see [`PristineBatch`]).
    ///
    /// Requires the WC write lock, like [`Self::install`].
    pub fn batch(&self) -> Result<PristineBatch, PristineError> {
        self.require_writable()?;
        let tmp_dir = self.adm_dir.join("tmp");
        fs::create_dir_all(&tmp_dir).map_err(PristineError::io("create directory", &tmp_dir))?;
        Ok(PristineBatch {
            adm_dir: self.adm_dir.clone(),
        })
    }

    /// Opens the stored text for reading. Fails with [`PristineError::NotHydrated`] when the row says the
    /// text was dropped, and [`PristineError::NotInstalled`] when row or file is missing.
    pub fn open_text(&self, sha1: &Sha1Digest) -> Result<File, PristineError> {
        self.open_text_with_row(sha1).map(|(_, file)| file)
    }

    /// [`open_text`](Self::open_text) that also returns the row it looked up (one lookup when
    /// the caller needs both, for example to check the MD5 a delta expects).
    pub fn open_text_with_row(
        &self,
        sha1: &Sha1Digest,
    ) -> Result<(PristineRecord, File), PristineError> {
        let row = self
            .index
            .lookup(sha1)?
            .ok_or(PristineError::NotInstalled(*sha1))?;
        if !row.hydrated {
            return Err(PristineError::NotHydrated(*sha1));
        }
        let path = self.path_of(sha1);
        let file = File::open(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => PristineError::NotInstalled(*sha1),
            _ => PristineError::Io {
                op: "open",
                path,
                source: e,
            },
        })?;
        Ok((row, file))
    }

    /// Checks the stored text against the row's size and its SHA-1; `true` when both match.
    pub fn verify(&self, sha1: &Sha1Digest) -> Result<bool, PristineError> {
        let row = self
            .index
            .lookup(sha1)?
            .ok_or(PristineError::NotInstalled(*sha1))?;
        let mut file = self.open_text(sha1)?;
        let len = file
            .metadata()
            .map_err(PristineError::io("stat", &self.path_of(sha1)))?
            .len();
        if len != row.size {
            return Ok(false);
        }
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

    /// Removes the texts nobody references any more (cleanup) and returns how many were removed.
    ///
    /// For each candidate the row is deleted first, atomically and only if it is still
    /// unreferenced ([`PristineIndex::forget_if_unreferenced`]); the file is removed only after
    /// that, so a text that gained a reference in the meantime is never deleted. A missing file is
    /// not an error. Requires the WC write lock.
    pub fn remove_unreferenced(&mut self) -> Result<usize, PristineError> {
        self.require_writable()?;
        let mut removed = 0;
        for sha1 in self.index.unreferenced()? {
            if !self.index.forget_if_unreferenced(&sha1)? {
                continue;
            }
            removed += 1;
            let path = self.path_of(&sha1);
            clear_read_only_for_delete(&path)?;
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
        }
        Ok(removed)
    }
}

/// Makes sure the text of `record` is in the store below `adm_dir`, for a transaction about to
/// reference it. Returns `Ok(false)` when the file was there, `Ok(true)` when it was missing and
/// was restored from `spare` (a file holding exactly that text, hashed when it was written;
/// synced, renamed into place, made read-only, directory synced, so it is durable before the
/// transaction commits), and [`PristineError::NotInstalled`] when it is missing and no spare
/// is left. Never removes anything.
pub(crate) fn ensure_stored(
    adm_dir: &Path,
    record: &PristineRecord,
    spare: Option<&Path>,
) -> Result<bool, PristineError> {
    let dest = text_path(adm_dir, &record.sha1);
    if fs::symlink_metadata(&dest).is_ok_and(|m| m.file_type().is_file()) {
        return Ok(false);
    }
    let Some(src) = spare.filter(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_file())) else {
        return Err(PristineError::NotInstalled(record.sha1));
    };
    let file = File::open(src).map_err(PristineError::io("open", src))?;
    let len = file
        .metadata()
        .map_err(PristineError::io("stat", src))?
        .len();
    if len != record.size {
        return Err(PristineError::ChecksumMismatch {
            expected: format!("{} bytes", record.size),
            actual: format!("{len} bytes"),
        });
    }
    drop(file);
    crate::durable::sync_file(src).map_err(PristineError::io("sync", src))?;
    let parent = dest.parent().unwrap_or(adm_dir);
    fs::create_dir_all(parent).map_err(PristineError::io("create directory", parent))?;
    rename_replacing(src, &dest)?;
    let mut perms = fs::metadata(&dest)
        .map_err(PristineError::io("stat", &dest))?
        .permissions();
    perms.set_readonly(true);
    fs::set_permissions(&dest, perms).map_err(PristineError::io("set permissions of", &dest))?;
    sync_dir(parent)?;
    Ok(true)
}

/// Where the text with checksum `sha1` lives below the administrative directory `adm_dir`.
fn text_path(adm_dir: &Path, sha1: &Sha1Digest) -> PathBuf {
    let hex = sha1.to_hex();
    adm_dir
        .join("pristine")
        .join(&hex[..2])
        .join(format!("{hex}{SUFFIX}"))
}

/// Streams `reader` into a new temp file in `tmp_dir` and returns it with its record.
fn write_temp(
    tmp_dir: &Path,
    reader: &mut dyn Read,
) -> Result<(tempfile::NamedTempFile, PristineRecord), PristineError> {
    let mut tmp = tempfile::Builder::new()
        .prefix("pristine-")
        .tempfile_in(tmp_dir)
        .map_err(PristineError::io("create temp file in", tmp_dir))?;
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
    Ok((tmp, record))
}

fn check_expected(
    record: &PristineRecord,
    expected_sha1: Option<&Sha1Digest>,
    expected_md5: Option<&Md5Digest>,
) -> Result<(), PristineError> {
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
    Ok(())
}

/// Syncs the temp file and renames it to `dest` (replacing an orphan file without a row, which
/// may be truncated or damaged), then makes it read-only. The directory entry is not synced.
fn place(tmp: tempfile::NamedTempFile, dest: &Path, adm_dir: &Path) -> Result<(), PristineError> {
    tmp.as_file()
        .sync_all()
        .map_err(PristineError::io("sync", tmp.path()))?;
    let parent = dest.parent().unwrap_or(adm_dir);
    fs::create_dir_all(parent).map_err(PristineError::io("create directory", parent))?;
    let tmp_path = tmp.into_temp_path();
    #[cfg(feature = "failpoints")]
    fail::fail_point!("pristine.install.before_rename", |_| Err(
        PristineError::index("failpoint pristine.install.before_rename")
    ));
    // On Windows this clears the read-only attribute and retries sharing violations.
    rename_replacing(&tmp_path, dest)?;
    // A pristine is immutable, like svn leaves it.
    let mut perms = fs::metadata(dest)
        .map_err(PristineError::io("stat", dest))?
        .permissions();
    perms.set_readonly(true);
    fs::set_permissions(dest, perms).map_err(PristineError::io("set permissions of", dest))
}

/// Puts many texts into a store with one directory sync per pristine directory and no row
/// written: the caller records the rows of all of them in one database transaction (for
/// example with [`crate::db::WcTxn::record_pristine`]) after [`Self::sync_dirs`].
///
/// The ordering of [`PristineStore::install`] holds for the batch as a whole: every text is
/// synced and in place before its row exists, so a crash leaves at worst orphan files, which a
/// later install of the same text replaces. [`Self::stage`] needs no `&mut` and the batch is
/// `Send + Sync`, so texts can be staged from several threads.
#[derive(Debug, Clone)]
pub struct PristineBatch {
    adm_dir: PathBuf,
}

impl PristineBatch {
    /// Where the text with this checksum lives (or would live).
    pub fn path_of(&self, sha1: &Sha1Digest) -> PathBuf {
        text_path(&self.adm_dir, sha1)
    }

    /// Streams `reader` into the store like [`PristineStore::install`] (checked against the
    /// expected checksums, synced, renamed into place, read-only), but without looking at or
    /// writing the index and without syncing the directory. The caller decides beforehand
    /// whether the text is needed (an existing hydrated row is authoritative).
    pub fn stage(
        &self,
        reader: &mut dyn Read,
        expected_sha1: Option<&Sha1Digest>,
        expected_md5: Option<&Md5Digest>,
    ) -> Result<PristineRecord, PristineError> {
        let (tmp, record) = write_temp(&self.adm_dir.join("tmp"), reader)?;
        check_expected(&record, expected_sha1, expected_md5)?;
        place(tmp, &self.path_of(&record.sha1), &self.adm_dir)?;
        Ok(record)
    }

    /// Moves the file at `src` into the store as the text with the expected checksums, without
    /// copying it when it is on the same file system: the content is hashed and checked first,
    /// then the file is renamed into place and made read-only. On another file system (or when
    /// the rename fails for another reason) it is copied like [`Self::stage`] and `src` stays.
    ///
    /// `src` must already be synced to disk: no sync happens here on the rename path.
    pub fn adopt(
        &self,
        src: &Path,
        expected_sha1: &Sha1Digest,
        expected_md5: &Md5Digest,
    ) -> Result<PristineRecord, PristineError> {
        let mut file = File::open(src).map_err(PristineError::io("open", src))?;
        let (mut sha, mut md5) = (Sha1::new(), Md5::new());
        let mut size = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file
                .read(&mut buf)
                .map_err(PristineError::io("read", src))?;
            if n == 0 {
                break;
            }
            sha.update(&buf[..n]);
            md5.update(&buf[..n]);
            size += n as u64;
        }
        drop(file);
        let record = PristineRecord {
            sha1: Sha1Digest(sha.finalize().into()),
            md5: Md5Digest(md5.finalize().into()),
            size,
            hydrated: true,
        };
        check_expected(&record, Some(expected_sha1), Some(expected_md5))?;
        self.place_file(src, &record)
    }

    /// [`Self::adopt`] for a file whose content the caller produced and hashed itself (for
    /// example a copy it wrote and synced moments ago): only its size is checked against
    /// `record`, the content is not read again.
    pub fn adopt_trusted(
        &self,
        src: &Path,
        record: &PristineRecord,
    ) -> Result<PristineRecord, PristineError> {
        let len = fs::metadata(src)
            .map_err(PristineError::io("stat", src))?
            .len();
        if len != record.size {
            return Err(PristineError::ChecksumMismatch {
                expected: format!("{} bytes", record.size),
                actual: format!("{len} bytes"),
            });
        }
        self.place_file(src, record)
    }

    /// Renames `src` into place as `record`'s text (copying when the rename fails).
    fn place_file(
        &self,
        src: &Path,
        record: &PristineRecord,
    ) -> Result<PristineRecord, PristineError> {
        let record = record.clone();
        let (expected_sha1, expected_md5) = (&record.sha1, &record.md5);
        let dest = self.path_of(&record.sha1);
        let parent = dest.parent().unwrap_or(&self.adm_dir);
        fs::create_dir_all(parent).map_err(PristineError::io("create directory", parent))?;
        #[cfg(feature = "failpoints")]
        fail::fail_point!("pristine.install.before_rename", |_| Err(
            PristineError::index("failpoint pristine.install.before_rename")
        ));
        if rename_replacing(src, &dest).is_err() {
            let mut file = File::open(src).map_err(PristineError::io("open", src))?;
            return self.stage(&mut file, Some(expected_sha1), Some(expected_md5));
        }
        let mut perms = fs::metadata(&dest)
            .map_err(PristineError::io("stat", &dest))?
            .permissions();
        perms.set_readonly(true);
        fs::set_permissions(&dest, perms)
            .map_err(PristineError::io("set permissions of", &dest))?;
        Ok(record)
    }

    /// Syncs the directory of every staged text once; call it before recording their rows.
    pub fn sync_dirs(&self, records: &[PristineRecord]) -> Result<(), PristineError> {
        let dirs: std::collections::BTreeSet<PathBuf> = records
            .iter()
            .filter_map(|r| self.path_of(&r.sha1).parent().map(Path::to_path_buf))
            .collect();
        for d in dirs {
            sync_dir(&d)?;
        }
        Ok(())
    }
}

/// Windows refuses to delete read-only files; Unix deletes by directory permission.
///
/// Unverified on Windows (no Windows CI).
#[cfg(windows)]
fn clear_read_only_for_delete(path: &Path) -> Result<(), PristineError> {
    match fs::metadata(path) {
        Ok(meta) => {
            let mut perms = meta.permissions();
            // Windows only: this clears FILE_ATTRIBUTE_READONLY, it does not make the file
            // world-writable.
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            fs::set_permissions(path, perms).map_err(PristineError::io("clear read-only of", path))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(PristineError::Io {
            op: "stat",
            path: path.to_path_buf(),
            source: e,
        }),
    }
}

#[cfg(not(windows))]
fn clear_read_only_for_delete(_path: &Path) -> Result<(), PristineError> {
    Ok(())
}

/// Flushes a directory entry (the rename) to disk. Windows cannot open directories for sync.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<(), PristineError> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(PristineError::io("sync directory", dir))
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> Result<(), PristineError> {
    Ok(())
}

#[cfg(test)]
mod tests;
