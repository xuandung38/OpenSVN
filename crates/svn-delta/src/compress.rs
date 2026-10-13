//! Per-section compression of svndiff 1 (zlib) and svndiff 2 (LZ4).
//!
//! Layout, verified against real `svnadmin` output: the window header's instruction and new-data
//! lengths cover the *whole* stored section, which is `varint(original_len)` followed by the
//! payload. The payload is the raw bytes when `payload.len() == original_len` (compression did not
//! help) and the compressed stream otherwise.

use std::borrow::Cow;
use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use svn_types::{ErrorCode, Result, SvnError};

use crate::encoder::SvndiffVersion;
use crate::varint::{decode_varint, encode_varint};

/// Sections shorter than this are stored raw, as Subversion does.
const MIN_COMPRESS_SIZE: usize = 512;

fn corrupt(msg: &str) -> SvnError {
    SvnError::new(ErrorCode::Corrupt, msg)
}

/// Returns the decoded content of one stored section.
///
/// `max_len` bounds the declared original length before any allocation happens.
pub(crate) fn decode_section(
    version: SvndiffVersion,
    stored: &[u8],
    max_len: usize,
) -> Result<Cow<'_, [u8]>> {
    if version == SvndiffVersion::V0 {
        return Ok(Cow::Borrowed(stored));
    }
    if stored.is_empty() {
        return Ok(Cow::Borrowed(stored));
    }
    let (original, used) = decode_varint(stored)?
        .ok_or_else(|| corrupt("svndiff section ends inside its length prefix"))?;
    let original = usize::try_from(original)
        .ok()
        .filter(|n| *n <= max_len)
        .ok_or_else(|| corrupt("svndiff section declares an oversized length"))?;
    let payload = &stored[used..];
    if payload.len() == original {
        return Ok(Cow::Borrowed(payload));
    }
    if payload.len() > original {
        return Err(corrupt("svndiff compressed section larger than original"));
    }
    let out = match version {
        SvndiffVersion::V1 => zlib_inflate(payload, original)?,
        SvndiffVersion::V2 => lz4_inflate(payload, original)?,
        SvndiffVersion::V0 => return Ok(Cow::Borrowed(stored)),
    };
    Ok(Cow::Owned(out))
}

fn zlib_inflate(payload: &[u8], original: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(original);
    // Read one byte past the declared size so an over-long stream is detected, never buffered.
    let limit = original as u64 + 1;
    ZlibDecoder::new(payload)
        .take(limit)
        .read_to_end(&mut out)
        .map_err(|e| corrupt(&format!("svndiff zlib section is invalid: {e}")))?;
    if out.len() != original {
        return Err(corrupt("svndiff zlib section has the wrong length"));
    }
    Ok(out)
}

fn lz4_inflate(payload: &[u8], original: usize) -> Result<Vec<u8>> {
    let out = lz4_flex::block::decompress(payload, original)
        .map_err(|e| corrupt(&format!("svndiff lz4 section is invalid: {e}")))?;
    if out.len() != original {
        return Err(corrupt("svndiff lz4 section has the wrong length"));
    }
    Ok(out)
}

/// Appends the stored form of `data` for `version` to `out`.
pub(crate) fn encode_section(
    version: SvndiffVersion,
    data: &[u8],
    out: &mut Vec<u8>,
) -> Result<()> {
    if version == SvndiffVersion::V0 {
        out.extend_from_slice(data);
        return Ok(());
    }
    encode_varint(data.len() as u64, out);
    if data.len() >= MIN_COMPRESS_SIZE {
        let packed = match version {
            SvndiffVersion::V1 => {
                let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
                enc.write_all(data)
                    .map_err(|e| SvnError::from_io("zlib compression", &e))?;
                enc.finish()
                    .map_err(|e| SvnError::from_io("zlib compression", &e))?
            }
            _ => lz4_flex::block::compress(data),
        };
        if packed.len() < data.len() {
            out.extend_from_slice(&packed);
            return Ok(());
        }
    }
    out.extend_from_slice(data);
    Ok(())
}
