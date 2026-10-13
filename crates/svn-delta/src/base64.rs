//! Streaming base64 adapter for svndiff carried inside XML (`txdelta` elements).

use ::base64::Engine;
use ::base64::engine::general_purpose::STANDARD;
use svn_types::{ErrorCode, Result, SvnError, TxDeltaWindow};

use crate::decoder::SvndiffDecoder;

fn corrupt(msg: &str) -> SvnError {
    SvnError::new(ErrorCode::Corrupt, msg)
}

/// Incremental base64 decoder. Text may be split anywhere, and ASCII whitespace (line breaks that
/// XML producers insert) is ignored.
///
/// Each call strips whitespace in one pass and decodes the whole run of complete 4-character
/// groups with a single engine call; at most 3 characters are carried to the next call.
#[derive(Debug, Default)]
pub struct Base64Decoder {
    /// Reusable work area: carried characters followed by the cleaned chunk.
    work: Vec<u8>,
    /// Number of leading `work` bytes that are carried characters (always < 4 between calls).
    carried: usize,
    /// A padded group was seen; nothing but whitespace may follow.
    ended: bool,
}

impl Base64Decoder {
    /// Creates an empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decodes the next piece of base64 text, returning the bytes completed so far.
    pub fn feed(&mut self, text: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.feed_into(text, &mut out)?;
        Ok(out)
    }

    /// Like [`feed`](Self::feed) but appends the decoded bytes to `out`, so a caller can reuse
    /// one buffer across calls.
    pub fn feed_into(&mut self, text: &[u8], out: &mut Vec<u8>) -> Result<()> {
        let tail = text.last().is_some_and(u8::is_ascii_whitespace);
        let trimmed = if tail {
            let end = text
                .iter()
                .rposition(|c| !c.is_ascii_whitespace())
                .map_or(0, |i| i + 1);
            &text[..end]
        } else {
            text
        };
        if trimmed.is_empty() {
            return Ok(());
        }
        if self.ended {
            return Err(corrupt("base64 data continues after padding"));
        }
        // Fast path: nothing carried and no interior whitespace, so decode in place.
        if self.carried == 0 && !trimmed.iter().any(u8::is_ascii_whitespace) {
            let full = trimmed.len() / 4 * 4;
            self.decode_groups(&trimmed[..full], trimmed.len() == full, out)?;
            self.work.clear();
            self.work.extend_from_slice(&trimmed[full..]);
            self.carried = self.work.len();
            return Ok(());
        }
        // Slow path: strip whitespace into the work area behind the carried characters.
        self.work.truncate(self.carried);
        for run in trimmed.split(|c| c.is_ascii_whitespace()) {
            self.work.extend_from_slice(run);
        }
        let full = self.work.len() / 4 * 4;
        let work = std::mem::take(&mut self.work);
        let res = self.decode_groups(&work[..full], work.len() == full, out);
        self.work = work;
        res?;
        self.work.copy_within(full.., 0);
        self.carried = self.work.len() - full;
        self.work.truncate(self.carried);
        Ok(())
    }

    /// Decodes whole 4-character groups. `last` tells whether they are the end of the available
    /// text, the only place padding may appear.
    fn decode_groups(&mut self, groups: &[u8], last: bool, out: &mut Vec<u8>) -> Result<()> {
        STANDARD
            .decode_vec(groups, out)
            .map_err(|e| corrupt(&format!("invalid base64: {e}")))?;
        if groups.ends_with(b"=") {
            if !last {
                return Err(corrupt("base64 data continues after padding"));
            }
            self.ended = true;
        }
        Ok(())
    }

    /// Declares end of text; a dangling partial group is `Corrupt`.
    pub fn finish(&mut self) -> Result<()> {
        if self.carried == 0 {
            Ok(())
        } else {
            Err(corrupt("base64 data ends inside a group"))
        }
    }
}

/// Base64 text in, svndiff windows out.
#[derive(Debug, Default)]
pub struct Base64SvndiffDecoder {
    base64: Base64Decoder,
    scratch: Vec<u8>,
    svndiff: SvndiffDecoder,
}

impl Base64SvndiffDecoder {
    /// Creates a decoder at the start of a base64-wrapped svndiff stream.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds the next piece of base64 text and returns completed windows.
    pub fn feed(&mut self, text: &[u8]) -> Result<Vec<TxDeltaWindow>> {
        self.scratch.clear();
        self.base64.feed_into(text, &mut self.scratch)?;
        self.svndiff.feed(&self.scratch)
    }

    /// Declares end of text and of the svndiff stream.
    pub fn finish(&mut self) -> Result<()> {
        self.base64.finish()?;
        self.svndiff.finish()
    }
}
