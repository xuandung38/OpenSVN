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
#[derive(Debug, Default)]
pub struct Base64Decoder {
    /// Characters of an incomplete 4-character group.
    group: Vec<u8>,
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
        let mut out = Vec::with_capacity(text.len() / 4 * 3);
        for &c in text {
            if c.is_ascii_whitespace() {
                continue;
            }
            if self.ended {
                return Err(corrupt("base64 data continues after padding"));
            }
            self.group.push(c);
            if self.group.len() == 4 {
                STANDARD
                    .decode_vec(&self.group, &mut out)
                    .map_err(|e| corrupt(&format!("invalid base64: {e}")))?;
                self.ended = self.group.contains(&b'=');
                self.group.clear();
            }
        }
        Ok(out)
    }

    /// Declares end of text; a dangling partial group is `Corrupt`.
    pub fn finish(&mut self) -> Result<()> {
        if self.group.is_empty() {
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
    svndiff: SvndiffDecoder,
}

impl Base64SvndiffDecoder {
    /// Creates a decoder at the start of a base64-wrapped svndiff stream.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds the next piece of base64 text and returns completed windows.
    pub fn feed(&mut self, text: &[u8]) -> Result<Vec<TxDeltaWindow>> {
        let bytes = self.base64.feed(text)?;
        self.svndiff.feed(&bytes)
    }

    /// Declares end of text and of the svndiff stream.
    pub fn finish(&mut self) -> Result<()> {
        self.base64.finish()?;
        self.svndiff.finish()
    }
}
