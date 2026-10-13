//! Window instruction codec and structural validation.
//!
//! An instruction is one byte (`op << 6 | length`), followed by a varint length when the 6-bit
//! length is zero, followed by a varint offset for the two copy ops.

use svn_types::{ErrorCode, Result, SvnError, TxDeltaOp, TxDeltaWindow};

use crate::varint::{decode_varint, encode_varint};

/// Upper bound for the instruction section of one window (after decompression).
pub(crate) const MAX_INSTRUCTIONS_LEN: usize = 4 * 1024 * 1024;
/// Upper bound for source view, target view and new-data lengths of one window.
pub(crate) const MAX_VIEW_LEN: usize = 64 * 1024 * 1024;

fn corrupt(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::Corrupt, msg)
}

/// Decodes an instruction section into ops and validates them against the window geometry.
///
/// `NewData` offsets are assigned sequentially, as the wire format has none.
pub fn parse_instructions(
    ins: &[u8],
    sview_len: u64,
    tview_len: u64,
    new_len: u64,
) -> Result<Vec<TxDeltaOp>> {
    let mut ops = Vec::new();
    let mut pos = 0;
    let mut new_pos: u64 = 0;
    while pos < ins.len() {
        let head = ins[pos];
        pos += 1;
        let mut length = u64::from(head & 0x3f);
        if length == 0 {
            let (v, used) = decode_varint(&ins[pos..])?
                .ok_or_else(|| corrupt("svndiff instruction length is truncated"))?;
            length = v;
            pos += used;
        }
        let op = match head >> 6 {
            0 | 1 => {
                let (offset, used) = decode_varint(&ins[pos..])?
                    .ok_or_else(|| corrupt("svndiff instruction offset is truncated"))?;
                pos += used;
                if head >> 6 == 0 {
                    TxDeltaOp::CopyFromSource { offset, length }
                } else {
                    TxDeltaOp::CopyFromTarget { offset, length }
                }
            }
            2 => {
                let offset = new_pos;
                new_pos = new_pos
                    .checked_add(length)
                    .ok_or_else(|| corrupt("svndiff new-data length overflows"))?;
                TxDeltaOp::NewData { offset, length }
            }
            _ => return Err(corrupt("svndiff instruction has invalid opcode 3")),
        };
        ops.push(op);
    }
    validate_ops(&ops, sview_len, tview_len, new_len)?;
    Ok(ops)
}

/// Encodes ops into an instruction section. Zero-length ops are dropped.
pub fn serialize_instructions(ops: &[TxDeltaOp], out: &mut Vec<u8>) {
    for op in ops {
        let (code, length, offset) = match *op {
            TxDeltaOp::CopyFromSource { offset, length } => (0u8, length, Some(offset)),
            TxDeltaOp::CopyFromTarget { offset, length } => (1u8, length, Some(offset)),
            TxDeltaOp::NewData { length, .. } => (2u8, length, None),
        };
        if length == 0 {
            continue;
        }
        if length < 64 {
            // length < 64 fits the 6-bit field.
            out.push((code << 6) | length as u8);
        } else {
            out.push(code << 6);
            encode_varint(length, out);
        }
        if let Some(offset) = offset {
            encode_varint(offset, out);
        }
    }
}

fn validate_ops(ops: &[TxDeltaOp], sview_len: u64, tview_len: u64, new_len: u64) -> Result<()> {
    let mut tpos: u64 = 0;
    let mut npos: u64 = 0;
    for op in ops {
        let length = match *op {
            TxDeltaOp::CopyFromSource { offset, length } => {
                let end = offset
                    .checked_add(length)
                    .ok_or_else(|| corrupt("svndiff source copy overflows"))?;
                if end > sview_len {
                    return Err(corrupt("svndiff source copy exceeds the source view"));
                }
                length
            }
            TxDeltaOp::CopyFromTarget { offset, length } => {
                if offset >= tpos {
                    return Err(corrupt(
                        "svndiff target copy starts beyond the written data",
                    ));
                }
                length
            }
            TxDeltaOp::NewData { offset, length } => {
                if offset != npos {
                    return Err(corrupt("svndiff new-data ops are not sequential"));
                }
                npos = npos
                    .checked_add(length)
                    .filter(|end| *end <= new_len)
                    .ok_or_else(|| corrupt("svndiff new-data copy exceeds the new data"))?;
                length
            }
        };
        tpos = tpos
            .checked_add(length)
            .filter(|end| *end <= tview_len)
            .ok_or_else(|| corrupt("svndiff instructions exceed the target view"))?;
    }
    if tpos != tview_len {
        return Err(corrupt("svndiff instructions do not fill the target view"));
    }
    Ok(())
}

/// Checks that a window is internally consistent: sizes within limits, every op inside its
/// source (source view, bytes already produced, new data) and the ops exactly fill the target
/// view. Failures are [`ErrorCode::Corrupt`].
pub fn validate_window(w: &TxDeltaWindow) -> Result<()> {
    let limit = MAX_VIEW_LEN as u64;
    if w.sview_len > limit || w.tview_len > limit || w.new_data.len() as u64 > limit {
        return Err(corrupt("svndiff window exceeds the size limit"));
    }
    w.sview_offset
        .checked_add(w.sview_len)
        .ok_or_else(|| corrupt("svndiff source view overflows"))?;
    validate_ops(&w.ops, w.sview_len, w.tview_len, w.new_data.len() as u64)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn hand_built_instructions_parse() {
        // copy-source len 3 @ 5; new len 2; copy-target len 70 @ 0 (overlapping)
        let ins = [0x03, 0x05, 0x82, 0x40, 70, 0x00];
        let ops = parse_instructions(&ins, 8, 3 + 2 + 70, 2).unwrap();
        assert_eq!(
            ops,
            vec![
                TxDeltaOp::CopyFromSource {
                    offset: 5,
                    length: 3
                },
                TxDeltaOp::NewData {
                    offset: 0,
                    length: 2
                },
                TxDeltaOp::CopyFromTarget {
                    offset: 0,
                    length: 70
                },
            ]
        );
        let mut out = Vec::new();
        serialize_instructions(&ops, &mut out);
        assert_eq!(out, ins);
    }

    #[test]
    fn rejects_bad_ops() {
        // opcode 3
        assert!(parse_instructions(&[0xc1], 0, 1, 0).is_err());
        // source copy past view
        assert!(parse_instructions(&[0x04, 0x00], 3, 4, 0).is_err());
        // target copy at tpos
        assert!(parse_instructions(&[0x41, 0x00], 0, 1, 0).is_err());
        // new data exceeds
        assert!(parse_instructions(&[0x85], 0, 5, 4).is_err());
        // does not fill target
        assert!(parse_instructions(&[0x82], 0, 5, 2).is_err());
        // truncated length varint
        assert!(parse_instructions(&[0x80], 0, 5, 5).is_err());
        // truncated offset
        assert!(parse_instructions(&[0x01], 1, 1, 0).is_err());
    }
}
