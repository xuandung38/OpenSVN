//! Window sink that rebuilds a file text into a temporary file next to the pristine store.

use std::fs::File;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::sync::PoisonError;

use svn_delta::{ApplyHandle, WindowApplier};
use svn_types::{ErrorCode, Result, SvnError, TxDeltaWindow, WindowSink};
use tempfile::TempPath;

use super::state::{NewText, TextSlot};

/// Base text of a delta: the pristine file or nothing.
pub enum BaseSource {
    /// Stored pristine text.
    File(File),
    /// Delta against the empty text.
    Empty(Cursor<Vec<u8>>),
}

impl Read for BaseSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::File(f) => f.read(buf),
            Self::Empty(c) => c.read(buf),
        }
    }
}

impl Seek for BaseSource {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        match self {
            Self::File(f) => f.seek(pos),
            Self::Empty(c) => c.seek(pos),
        }
    }
}

/// Applies windows onto `source`, writing the result to a temporary file; at the end of the
/// delta the digests and the file move into the shared slot.
pub struct TextSink {
    applier: WindowApplier<BaseSource, File>,
    handle: ApplyHandle,
    path: Option<TempPath>,
    slot: TextSlot,
}

impl TextSink {
    /// Creates a sink writing into `out` (kept alive through `path`).
    pub fn new(source: BaseSource, out: File, path: TempPath, slot: TextSlot) -> Self {
        let applier = WindowApplier::new(source, out);
        let handle = applier.handle();
        Self {
            applier,
            handle,
            path: Some(path),
            slot,
        }
    }
}

impl WindowSink for TextSink {
    fn window(&mut self, w: Option<&TxDeltaWindow>) -> Result<()> {
        match w {
            Some(w) => self.applier.apply_window(w),
            None => {
                self.applier.finish()?;
                let outcome = self.handle.outcome().ok_or_else(|| {
                    SvnError::new(ErrorCode::Internal, "text delta ended without a result")
                })?;
                let path = self
                    .path
                    .take()
                    .ok_or_else(|| SvnError::new(ErrorCode::Internal, "text delta ended twice"))?;
                *self.slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(NewText {
                    path,
                    md5: outcome.md5,
                    sha1: outcome.sha1,
                    size: outcome.bytes_written,
                });
                Ok(())
            }
        }
    }
}

/// Sink for a skipped file: accepts and drops everything.
pub struct DiscardSink;

impl WindowSink for DiscardSink {
    fn window(&mut self, _: Option<&TxDeltaWindow>) -> Result<()> {
        Ok(())
    }
}
