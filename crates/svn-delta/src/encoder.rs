//! svndiff writer. Only full-text deltas are produced (every byte travels as new data), which is
//! all a commit needs.

use bytes::Bytes;
use svn_types::{Result, TxDeltaWindow};

use crate::compress::encode_section;
use crate::varint::encode_varint;
use crate::window::{serialize_instructions, validate_window};

/// Standard window size used by Subversion (100 KiB of output per window).
pub const WINDOW_SIZE: usize = 100 * 1024;

/// svndiff stream version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SvndiffVersion {
    /// Uncompressed (`SVN\0`).
    V0,
    /// Sections compressed with zlib (`SVN\x01`).
    V1,
    /// Sections compressed with LZ4 (`SVN\x02`).
    V2,
}

impl SvndiffVersion {
    /// The version byte that follows the `SVN` magic.
    pub fn byte(self) -> u8 {
        match self {
            Self::V0 => 0,
            Self::V1 => 1,
            Self::V2 => 2,
        }
    }

    /// Parses the version byte.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::V0),
            1 => Some(Self::V1),
            2 => Some(Self::V2),
            _ => None,
        }
    }
}

/// Incremental svndiff writer: the 4-byte header is emitted before the first window.
#[derive(Debug)]
pub struct SvndiffEncoder {
    version: SvndiffVersion,
    header_written: bool,
}

impl SvndiffEncoder {
    /// Creates a writer for `version`.
    pub fn new(version: SvndiffVersion) -> Self {
        Self {
            version,
            header_written: false,
        }
    }

    /// Writes the header if it has not been written yet. Call this alone to produce the stream
    /// for an empty delta.
    pub fn write_header(&mut self, out: &mut Vec<u8>) {
        if !self.header_written {
            out.extend_from_slice(b"SVN");
            out.push(self.version.byte());
            self.header_written = true;
        }
    }

    /// Appends one window (and the header, if still pending) to `out`.
    ///
    /// Fails with `Corrupt` when the window is internally inconsistent.
    pub fn write_window(&mut self, w: &TxDeltaWindow, out: &mut Vec<u8>) -> Result<()> {
        validate_window(w)?;
        self.write_header(out);
        let mut ins = Vec::new();
        serialize_instructions(&w.ops, &mut ins);
        let mut ins_stored = Vec::new();
        encode_section(self.version, &ins, &mut ins_stored)?;
        let mut new_stored = Vec::new();
        encode_section(self.version, &w.new_data, &mut new_stored)?;
        encode_varint(w.sview_offset, out);
        encode_varint(w.sview_len, out);
        encode_varint(w.tview_len, out);
        encode_varint(ins_stored.len() as u64, out);
        encode_varint(new_stored.len() as u64, out);
        out.extend_from_slice(&ins_stored);
        out.extend_from_slice(&new_stored);
        Ok(())
    }

    /// Appends `data` as full-text windows of at most [`WINDOW_SIZE`] bytes.
    pub fn write_fulltext(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<()> {
        self.write_header(out);
        for chunk in data.chunks(WINDOW_SIZE) {
            self.write_window(
                &TxDeltaWindow::full_text(Bytes::copy_from_slice(chunk)),
                out,
            )?;
        }
        Ok(())
    }

    /// One-shot: the complete svndiff stream that rebuilds `data` from an empty base.
    /// Empty input yields only the header.
    pub fn fulltext(version: SvndiffVersion, data: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(data.len() / 2 + 16);
        Self::new(version).write_fulltext(data, &mut out)?;
        Ok(out)
    }
}
