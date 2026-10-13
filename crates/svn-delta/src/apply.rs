//! Applies text-delta windows onto a source stream, hashing what is produced.

use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex, PoisonError};

use md5::{Digest, Md5};
use sha1::Sha1;
use svn_types::{ErrorCode, Result, SvnError, TxDeltaOp, TxDeltaWindow, WindowSink};

use crate::window::validate_window;

/// Subversion's `SVN_ERR_CHECKSUM_MISMATCH`.
const SVN_ERR_CHECKSUM_MISMATCH: u32 = 200_014;

/// Result of a completed apply: size and digests of the produced text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// Number of bytes written to the output.
    pub bytes_written: u64,
    /// MD5 of the output (wire checksum).
    pub md5: [u8; 16],
    /// SHA-1 of the output (pristine store key).
    pub sha1: [u8; 20],
}

/// Shared slot that receives the [`ApplyOutcome`] once the end-of-delta marker is processed.
///
/// A [`WindowApplier`] is usually boxed into an editor as `dyn WindowSink`, so the caller keeps
/// this handle to read the digests afterwards.
#[derive(Debug, Clone, Default)]
pub struct ApplyHandle(Arc<Mutex<Option<ApplyOutcome>>>);

impl ApplyHandle {
    /// The outcome, or `None` while the delta has not ended successfully.
    pub fn outcome(&self) -> Option<ApplyOutcome> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, outcome: ApplyOutcome) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(outcome);
    }
}

/// [`WindowSink`] that rebuilds a text from a seekable source and writes it to `W`.
///
/// Memory use is one source view plus one output window. Instructions that reach outside their
/// view are `Corrupt`. Optional expected digests are verified when the delta ends.
pub struct WindowApplier<R, W> {
    source: R,
    output: W,
    md5: Md5,
    sha1: Sha1,
    written: u64,
    sview: Vec<u8>,
    out_buf: Vec<u8>,
    expected_md5: Option<[u8; 16]>,
    expected_sha1: Option<[u8; 20]>,
    handle: ApplyHandle,
    finished: bool,
}

impl<R: Read + Seek, W: Write> WindowApplier<R, W> {
    /// Creates an applier reading base text from `source` and writing the result to `output`.
    /// For a delta against nothing pass an empty reader such as `std::io::Cursor::new([])`.
    pub fn new(source: R, output: W) -> Self {
        Self {
            source,
            output,
            md5: Md5::new(),
            sha1: Sha1::new(),
            written: 0,
            sview: Vec::new(),
            out_buf: Vec::new(),
            expected_md5: None,
            expected_sha1: None,
            handle: ApplyHandle::default(),
            finished: false,
        }
    }

    /// Requires the output MD5 to equal `digest` when the delta ends.
    #[must_use]
    pub fn expect_md5(mut self, digest: [u8; 16]) -> Self {
        self.expected_md5 = Some(digest);
        self
    }

    /// Requires the output SHA-1 to equal `digest` when the delta ends.
    #[must_use]
    pub fn expect_sha1(mut self, digest: [u8; 20]) -> Self {
        self.expected_sha1 = Some(digest);
        self
    }

    /// Handle that yields the digests after the delta ended.
    pub fn handle(&self) -> ApplyHandle {
        self.handle.clone()
    }

    /// Applies one window.
    pub fn apply_window(&mut self, w: &TxDeltaWindow) -> Result<()> {
        if self.finished {
            return Err(SvnError::new(
                ErrorCode::Internal,
                "window delivered after the end of the delta",
            ));
        }
        validate_window(w)?;
        self.load_source_view(w)?;
        self.out_buf.clear();
        self.out_buf.reserve(w.tview_len as usize);
        for op in &w.ops {
            self.run_op(op, &w.new_data)?;
        }
        self.md5.update(&self.out_buf);
        self.sha1.update(&self.out_buf);
        self.output
            .write_all(&self.out_buf)
            .map_err(|e| SvnError::from_io("writing delta output", &e))?;
        self.written += self.out_buf.len() as u64;
        Ok(())
    }

    /// Ends the delta: flushes, checks expected digests, publishes the outcome and returns the
    /// output writer's final state through the handle.
    pub fn finish(&mut self) -> Result<ApplyOutcome> {
        if self.finished {
            return self.handle.outcome().ok_or_else(|| {
                SvnError::new(ErrorCode::Internal, "delta ended with a failure earlier")
            });
        }
        self.finished = true;
        self.output
            .flush()
            .map_err(|e| SvnError::from_io("flushing delta output", &e))?;
        let outcome = ApplyOutcome {
            bytes_written: self.written,
            md5: self.md5.clone().finalize().into(),
            sha1: self.sha1.clone().finalize().into(),
        };
        if self.expected_md5.is_some_and(|e| e != outcome.md5) {
            return Err(SvnError::new(
                ErrorCode::Corrupt,
                "MD5 checksum of the reconstructed text does not match",
            )
            .with_svn_code(SVN_ERR_CHECKSUM_MISMATCH));
        }
        if self.expected_sha1.is_some_and(|e| e != outcome.sha1) {
            return Err(SvnError::new(
                ErrorCode::Corrupt,
                "SHA-1 checksum of the reconstructed text does not match",
            )
            .with_svn_code(SVN_ERR_CHECKSUM_MISMATCH));
        }
        self.handle.set(outcome);
        Ok(outcome)
    }

    /// Consumes the applier and returns the output writer.
    pub fn into_output(self) -> W {
        self.output
    }

    fn load_source_view(&mut self, w: &TxDeltaWindow) -> Result<()> {
        self.sview.clear();
        if w.sview_len == 0 {
            return Ok(());
        }
        // validate_window bounded sview_len, so the cast cannot truncate.
        self.sview.resize(w.sview_len as usize, 0);
        self.source
            .seek(SeekFrom::Start(w.sview_offset))
            .map_err(|e| SvnError::from_io("seeking in delta base", &e))?;
        self.source.read_exact(&mut self.sview).map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                SvnError::new(
                    ErrorCode::Corrupt,
                    "delta source view extends past the base",
                )
            } else {
                SvnError::from_io("reading delta base", &e)
            }
        })
    }

    /// Runs one op. Bounds were verified by `validate_window`, but slicing stays checked so a
    /// logic slip becomes an error rather than a panic.
    fn run_op(&mut self, op: &TxDeltaOp, new_data: &[u8]) -> Result<()> {
        let bad = || SvnError::new(ErrorCode::Corrupt, "delta instruction out of range");
        match *op {
            TxDeltaOp::CopyFromSource { offset, length } => {
                let range = range_of(offset, length).ok_or_else(bad)?;
                self.out_buf
                    .extend_from_slice(self.sview.get(range).ok_or_else(bad)?);
            }
            TxDeltaOp::NewData { offset, length } => {
                let range = range_of(offset, length).ok_or_else(bad)?;
                self.out_buf
                    .extend_from_slice(new_data.get(range).ok_or_else(bad)?);
            }
            TxDeltaOp::CopyFromTarget { offset, length } => {
                // Byte-by-byte semantics: the copy may read bytes it has just produced. Copying
                // in chunks that only touch already-existing bytes yields the same result.
                let mut from = usize::try_from(offset).map_err(|_| bad())?;
                let mut remaining = usize::try_from(length).map_err(|_| bad())?;
                while remaining > 0 {
                    let available = self.out_buf.len().checked_sub(from).ok_or_else(bad)?;
                    let chunk = remaining.min(available);
                    if chunk == 0 {
                        return Err(bad());
                    }
                    self.out_buf.extend_from_within(from..from + chunk);
                    from += chunk;
                    remaining -= chunk;
                }
            }
        }
        Ok(())
    }
}

fn range_of(offset: u64, length: u64) -> Option<std::ops::Range<usize>> {
    let start = usize::try_from(offset).ok()?;
    let end = start.checked_add(usize::try_from(length).ok()?)?;
    Some(start..end)
}

impl<R: Read + Seek + Send, W: Write + Send> WindowSink for WindowApplier<R, W> {
    fn window(&mut self, w: Option<&TxDeltaWindow>) -> Result<()> {
        match w {
            Some(w) => self.apply_window(w),
            None => self.finish().map(|_| ()),
        }
    }
}
