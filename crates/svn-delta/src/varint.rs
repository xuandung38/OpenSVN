//! svndiff integers: big-endian base-128, high bit set on every byte except the last.

use svn_types::{ErrorCode, Result, SvnError};

/// Longest possible encoding of a `u64` (ceil(64 / 7)).
const MAX_VARINT_LEN: usize = 10;

/// Appends `value` to `out` using the svndiff integer encoding (130 -> `0x81 0x02`).
pub fn encode_varint(value: u64, out: &mut Vec<u8>) {
    let mut groups = 1;
    while groups < MAX_VARINT_LEN && (value >> (7 * groups)) != 0 {
        groups += 1;
    }
    for i in (0..groups).rev() {
        // Masked to 7 bits, so the narrowing is lossless.
        let mut byte = ((value >> (7 * i)) & 0x7f) as u8;
        if i != 0 {
            byte |= 0x80;
        }
        out.push(byte);
    }
}

/// Decodes one integer from the front of `buf`.
///
/// Returns `Ok(None)` when `buf` ends in the middle of the integer (more input needed) and
/// `Ok(Some((value, consumed)))` otherwise. Values that do not fit in `u64` are
/// [`ErrorCode::Corrupt`].
pub fn decode_varint(buf: &[u8]) -> Result<Option<(u64, usize)>> {
    let mut value: u64 = 0;
    for (i, &byte) in buf.iter().enumerate() {
        if value > (u64::MAX >> 7) {
            return Err(SvnError::new(
                ErrorCode::Corrupt,
                "svndiff integer overflows 64 bits",
            ));
        }
        value = (value << 7) | u64::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            return Ok(Some((value, i + 1)));
        }
        if i + 1 >= MAX_VARINT_LEN {
            return Err(SvnError::new(
                ErrorCode::Corrupt,
                "svndiff integer is too long",
            ));
        }
    }
    Ok(None)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn enc(v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        encode_varint(v, &mut out);
        out
    }

    #[test]
    fn boundary_values_roundtrip() {
        for v in [
            0,
            1,
            127,
            128,
            129,
            16_383,
            16_384,
            u64::from(u32::MAX),
            u64::MAX,
        ] {
            let bytes = enc(v);
            assert_eq!(decode_varint(&bytes).unwrap(), Some((v, bytes.len())));
        }
    }

    #[test]
    fn known_encodings() {
        assert_eq!(enc(0), [0x00]);
        assert_eq!(enc(127), [0x7f]);
        assert_eq!(enc(128), [0x81, 0x00]);
        assert_eq!(enc(130), [0x81, 0x02]);
        assert_eq!(enc(102_400), [0x86, 0xa0, 0x00]);
        assert_eq!(enc(u64::MAX).len(), 10);
    }

    #[test]
    fn incomplete_input_asks_for_more() {
        assert_eq!(decode_varint(&[]).unwrap(), None);
        assert_eq!(decode_varint(&[0x81]).unwrap(), None);
    }

    #[test]
    fn overflow_is_corrupt() {
        let too_big = [0xff; 11];
        assert!(decode_varint(&too_big).is_err());
        // 10 bytes but value exceeds u64.
        let mut over = vec![0xff; 9];
        over.push(0x7f);
        assert!(decode_varint(&over).is_err());
    }

    #[test]
    fn trailing_bytes_are_not_consumed() {
        assert_eq!(decode_varint(&[0x05, 0xaa]).unwrap(), Some((5, 1)));
    }
}
