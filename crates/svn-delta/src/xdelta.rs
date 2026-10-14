//! Delta computation: the window that rebuilds a target view from a source view, as Subversion's
//! `xdelta.c` builds it.
//!
//! The source view is indexed by a rolling Adler-32 checksum of its [`MATCH_BLOCKSIZE`]-byte
//! blocks at block-aligned offsets. The target is scanned byte by byte; a hit is checked
//! byte-for-byte, then extended backwards over the pending literal bytes and forwards as far as
//! both views agree, and becomes one `CopyFromSource` op. Everything between matches travels as
//! new data. Only source copies are produced: no `CopyFromTarget`, which keeps every window
//! valid for svndiff 0 readers and for servers that apply deltas strictly.

use bytes::Bytes;
use svn_types::{TxDeltaOp, TxDeltaWindow};

/// Block size of the source index (`MATCH_BLOCKSIZE` in svn's `xdelta.c`).
pub const MATCH_BLOCKSIZE: usize = 64;

/// Rolling Adler-32 over a fixed-size block (svn's `adler32.c` variant without the modulus).
#[derive(Clone, Copy)]
struct Adler {
    s1: u32,
    s2: u32,
}

impl Adler {
    fn of(block: &[u8]) -> Self {
        let mut a = Self { s1: 0, s2: 0 };
        for &b in block {
            a.s1 = a.s1.wrapping_add(u32::from(b));
            a.s2 = a.s2.wrapping_add(a.s1);
        }
        a
    }

    /// Slides the block one byte: `out` leaves, `inp` enters.
    fn roll(&mut self, out: u8, inp: u8) {
        self.s1 = self
            .s1
            .wrapping_sub(u32::from(out))
            .wrapping_add(u32::from(inp));
        self.s2 = self
            .s2
            .wrapping_sub((MATCH_BLOCKSIZE as u32).wrapping_mul(u32::from(out)))
            .wrapping_add(self.s1);
    }

    fn key(self) -> u32 {
        (self.s2 << 16) | (self.s1 & 0xffff)
    }
}

/// Source offsets of the block-aligned blocks, by checksum (open addressing; the first block
/// with a checksum wins, like svn's table).
struct BlockIndex {
    slots: Vec<(u32, u32)>,
    mask: usize,
}

const EMPTY: u32 = u32::MAX;

impl BlockIndex {
    fn new(source: &[u8]) -> Self {
        let blocks = source.len() / MATCH_BLOCKSIZE;
        let size = (blocks * 2).next_power_of_two().max(16);
        let mut idx = Self {
            slots: vec![(0, EMPTY); size],
            mask: size - 1,
        };
        for (i, block) in source.as_chunks::<MATCH_BLOCKSIZE>().0.iter().enumerate() {
            let key = Adler::of(block).key();
            let mut slot = Self::hash(key) & idx.mask;
            loop {
                let (k, pos) = idx.slots[slot];
                if pos == EMPTY {
                    // Windows are at most a few hundred KiB, far below u32::MAX.
                    idx.slots[slot] = (key, (i * MATCH_BLOCKSIZE) as u32);
                    break;
                }
                if k == key {
                    break;
                }
                slot = (slot + 1) & idx.mask;
            }
        }
        idx
    }

    fn hash(key: u32) -> usize {
        (key.wrapping_mul(0x9E37_79B1) >> 7) as usize
    }

    fn find(&self, key: u32) -> Option<usize> {
        let mut slot = Self::hash(key) & self.mask;
        loop {
            let (k, pos) = self.slots[slot];
            if pos == EMPTY {
                return None;
            }
            if k == key {
                return Some(pos as usize);
            }
            slot = (slot + 1) & self.mask;
        }
    }
}

/// Collects ops and literal bytes, merging adjacent ops of the same kind.
struct Builder<'t> {
    target: &'t [u8],
    ops: Vec<TxDeltaOp>,
    new_data: Vec<u8>,
}

impl Builder<'_> {
    fn insert(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        let length = (to - from) as u64;
        if let Some(TxDeltaOp::NewData { length: l, .. }) = self.ops.last_mut() {
            *l += length;
        } else {
            self.ops.push(TxDeltaOp::NewData {
                offset: self.new_data.len() as u64,
                length,
            });
        }
        self.new_data.extend_from_slice(&self.target[from..to]);
    }

    fn copy(&mut self, offset: usize, len: usize) {
        if len == 0 {
            return;
        }
        if let Some(TxDeltaOp::CopyFromSource {
            offset: o,
            length: l,
        }) = self.ops.last_mut()
            && *o + *l == offset as u64
        {
            *l += len as u64;
            return;
        }
        self.ops.push(TxDeltaOp::CopyFromSource {
            offset: offset as u64,
            length: len as u64,
        });
    }
}

/// Length of the common prefix of `a` and `b`.
fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    const CHUNK: usize = 16;
    let n = a.len().min(b.len());
    let mut i = 0;
    while i + CHUNK <= n && a[i..i + CHUNK] == b[i..i + CHUNK] {
        i += CHUNK;
    }
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

/// The window that rebuilds `target` from `source`, the source view starting at
/// `sview_offset` in the base text.
///
/// Identical views become a single copy. A view shorter than [`MATCH_BLOCKSIZE`] on either side
/// is sent as new data (the source view is then empty).
pub fn delta_window(sview_offset: u64, source: &[u8], target: &[u8]) -> TxDeltaWindow {
    if source == target && !target.is_empty() {
        return TxDeltaWindow {
            sview_offset,
            sview_len: source.len() as u64,
            tview_len: target.len() as u64,
            ops: vec![TxDeltaOp::CopyFromSource {
                offset: 0,
                length: target.len() as u64,
            }],
            new_data: Bytes::new(),
        };
    }
    if source.len() < MATCH_BLOCKSIZE || target.len() < MATCH_BLOCKSIZE {
        let mut w = TxDeltaWindow::full_text(Bytes::copy_from_slice(target));
        if target.is_empty() {
            w.ops.clear();
        }
        w.sview_offset = sview_offset;
        return w;
    }
    let index = BlockIndex::new(source);
    let mut b = Builder {
        target,
        ops: Vec::new(),
        new_data: Vec::new(),
    };
    // Bytes before `pending` are covered by ops; `pos` is the start of the rolling block.
    let mut pending = 0;
    let mut pos = 0;
    let mut sum = Adler::of(&target[..MATCH_BLOCKSIZE]);
    while pos + MATCH_BLOCKSIZE <= target.len() {
        let hit = index
            .find(sum.key())
            .filter(|&s| source[s..s + MATCH_BLOCKSIZE] == target[pos..pos + MATCH_BLOCKSIZE]);
        if let Some(spos) = hit {
            // Extend backwards over pending literals, then forwards.
            let mut back = 0;
            while back < pos - pending
                && back < spos
                && source[spos - back - 1] == target[pos - back - 1]
            {
                back += 1;
            }
            let (s0, t0) = (spos - back, pos - back);
            let len = common_prefix(&source[s0..], &target[t0..]);
            b.insert(pending, t0);
            b.copy(s0, len);
            pending = t0 + len;
            pos = pending;
            if pos + MATCH_BLOCKSIZE <= target.len() {
                sum = Adler::of(&target[pos..pos + MATCH_BLOCKSIZE]);
            }
            continue;
        }
        if pos + MATCH_BLOCKSIZE < target.len() {
            sum.roll(target[pos], target[pos + MATCH_BLOCKSIZE]);
        }
        pos += 1;
    }
    b.insert(pending, target.len());
    let uses_source = b
        .ops
        .iter()
        .any(|op| matches!(op, TxDeltaOp::CopyFromSource { .. }));
    TxDeltaWindow {
        sview_offset,
        sview_len: if uses_source { source.len() as u64 } else { 0 },
        tview_len: target.len() as u64,
        ops: b.ops,
        new_data: Bytes::from(b.new_data),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::window::validate_window;

    fn apply(source: &[u8], w: &TxDeltaWindow) -> Vec<u8> {
        validate_window(w).unwrap();
        let mut out = Vec::new();
        for op in &w.ops {
            match *op {
                TxDeltaOp::CopyFromSource { offset, length } => {
                    out.extend_from_slice(&source[offset as usize..(offset + length) as usize])
                }
                TxDeltaOp::NewData { offset, length } => {
                    out.extend_from_slice(&w.new_data[offset as usize..(offset + length) as usize])
                }
                TxDeltaOp::CopyFromTarget { .. } => panic!("no target copies expected"),
            }
        }
        out
    }

    fn noise(seed: u64, n: usize) -> Vec<u8> {
        let mut x = seed | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    #[test]
    fn one_byte_change_is_tiny() {
        let src = noise(1, 100 * 1024);
        let mut tgt = src.clone();
        tgt[50_000] ^= 0xff;
        let w = delta_window(7, &src, &tgt);
        assert_eq!(apply(&src, &w), tgt);
        assert_eq!(w.sview_offset, 7);
        assert!(w.new_data.len() <= 1, "{}", w.new_data.len());
        assert!(w.ops.len() <= 3);
    }

    #[test]
    fn insertions_deletions_and_moves_round_trip() {
        let src = noise(2, 80_000);
        let mut tgt = Vec::new();
        tgt.extend_from_slice(&src[10_000..30_000]);
        tgt.extend_from_slice(b"inserted text that is new");
        tgt.extend_from_slice(&src[..5_000]);
        tgt.extend_from_slice(&src[40_001..80_000]);
        let w = delta_window(0, &src, &tgt);
        assert_eq!(apply(&src, &w), tgt);
        assert!(w.new_data.len() < 200, "{}", w.new_data.len());
    }

    #[test]
    fn identical_unrelated_short_and_empty_views() {
        let src = noise(3, 5_000);
        let w = delta_window(0, &src, &src);
        assert_eq!(w.ops.len(), 1);
        assert_eq!(apply(&src, &w), src);
        let other = noise(4, 5_000);
        let w = delta_window(0, &src, &other);
        assert_eq!(apply(&src, &w), other);
        assert_eq!(w.sview_len, 0);
        let w = delta_window(0, &src, b"short");
        assert_eq!(apply(&src, &w), b"short");
        let w = delta_window(0, &src, b"");
        assert_eq!(apply(&src, &w), b"");
        let w = delta_window(0, b"", &src);
        assert_eq!(apply(b"", &w), src);
    }

    #[test]
    fn repetitive_text_round_trips() {
        let src: Vec<u8> = b"line\n".iter().copied().cycle().take(50_000).collect();
        let mut tgt = src.clone();
        tgt.splice(1000..1000, b"extra line\n".iter().copied());
        tgt.truncate(45_000);
        let w = delta_window(0, &src, &tgt);
        assert_eq!(apply(&src, &w), tgt);
    }
}
