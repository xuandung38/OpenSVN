//! Bounded access to the parts of an Office Open XML package (a zip archive).

use std::cell::Cell;
use std::io::{BufRead, BufReader, Read, Seek};
use std::rc::Rc;

use zip::ZipArchive;
use zip::result::ZipError;

use crate::{ExtractError, Limits};

/// An opened package with the decompression budget of one extraction.
pub(crate) struct Package<R: Read + Seek> {
    archive: ZipArchive<R>,
    limits: Limits,
    /// Decompressed bytes read so far from all parts.
    total: u64,
}

impl<R: Read + Seek> Package<R> {
    /// Opens the zip directory of `reader`.
    pub(crate) fn open(reader: R, limits: &Limits) -> Result<Self, ExtractError> {
        let archive = ZipArchive::new(reader).map_err(|e| match e {
            ZipError::Io(e) => ExtractError::Io(e.to_string()),
            _ => ExtractError::NotAPackage,
        })?;
        if archive.len() > limits.max_entries {
            return Err(ExtractError::LimitExceeded(
                "too many entries in the package",
            ));
        }
        Ok(Self {
            archive,
            limits: *limits,
            total: 0,
        })
    }

    /// Deepest XML nesting allowed in a part.
    pub(crate) fn max_depth(&self) -> usize {
        self.limits.max_depth
    }

    /// Most shared strings of a workbook.
    pub(crate) fn max_shared_strings(&self) -> usize {
        self.limits.max_shared_strings
    }

    /// Whether the package has the part `name`.
    pub(crate) fn has_part(&self, name: &str) -> bool {
        self.resolve(name).is_some()
    }

    /// Names of all parts, in directory order.
    pub(crate) fn names(&self) -> Vec<String> {
        self.archive.file_names().map(str::to_owned).collect()
    }

    /// Exact name of the part `name` (part names are case-insensitive in a package).
    fn resolve(&self, name: &str) -> Option<String> {
        if self.archive.index_for_name(name).is_some() {
            return Some(name.to_owned());
        }
        self.archive
            .file_names()
            .find(|n| n.eq_ignore_ascii_case(name))
            .map(str::to_owned)
    }

    /// Runs `f` on a buffered, size-bounded reader of the part `name`; `Ok(None)` when the
    /// package has no such part. Reading past the per-part or the total decompression budget
    /// fails with [`ExtractError::LimitExceeded`], whatever sizes the zip headers claim.
    pub(crate) fn with_part<T>(
        &mut self,
        name: &str,
        f: impl FnOnce(&mut dyn BufRead) -> Result<T, ExtractError>,
    ) -> Result<Option<T>, ExtractError> {
        let Some(name) = self.resolve(name) else {
            return Ok(None);
        };
        let remaining_total = self.limits.max_total_bytes.saturating_sub(self.total);
        let entry = self.archive.by_name(&name).map_err(|e| match e {
            ZipError::UnsupportedArchive(m) if m.contains("assword") => {
                ExtractError::Encrypted(name.clone())
            }
            ZipError::Io(e) => ExtractError::Io(e.to_string()),
            other => ExtractError::Io(other.to_string()),
        })?;
        if entry.encrypted() {
            return Err(ExtractError::Encrypted(name));
        }
        let read = Rc::new(Cell::new(0u64));
        let over = Rc::new(Cell::new(None));
        let bounded = Bounded {
            inner: entry,
            read: Rc::clone(&read),
            over: Rc::clone(&over),
            entry_limit: self.limits.max_entry_bytes,
            total_limit: remaining_total,
        };
        let mut buffered = BufReader::new(bounded);
        let outcome = f(&mut buffered);
        drop(buffered);
        self.total = self.total.saturating_add(read.get());
        if let Some(what) = over.get() {
            return Err(ExtractError::LimitExceeded(what));
        }
        outcome.map(Some)
    }
}

/// Reader that fails once more than its budget of bytes has been produced, and records why.
struct Bounded<R> {
    inner: R,
    read: Rc<Cell<u64>>,
    over: Rc<Cell<Option<&'static str>>>,
    entry_limit: u64,
    total_limit: u64,
}

impl<R: Read> Read for Bounded<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        let read = self.read.get().saturating_add(n as u64);
        self.read.set(read);
        let over = if read > self.entry_limit {
            Some("a part decompresses to more than the allowed size")
        } else if read > self.total_limit {
            Some("the package decompresses to more than the allowed size")
        } else {
            None
        };
        if let Some(what) = over {
            self.over.set(Some(what));
            return Err(std::io::Error::other(what));
        }
        Ok(n)
    }
}
