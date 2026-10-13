// ====================================================================
//    Licensed to the Apache Software Foundation (ASF) under one
//    or more contributor license agreements.  See the NOTICE file
//    distributed with this work for additional information
//    regarding copyright ownership.  The ASF licenses this file
//    to you under the Apache License, Version 2.0 (the
//    "License"); you may not use this file except in compliance
//    with the License.  You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
//    Unless required by applicable law or agreed to in writing,
//    software distributed under the License is distributed on an
//    "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
//    KIND, either express or implied.  See the License for the
//    specific language governing permissions and limitations
//    under the License.
// ====================================================================
//
// Modified by OpenSVN: C pointer rings and pools replaced by index arenas; cancellation/budget checks added.

//! Longest common subsequence over token position rings.
//!
//! Faithful port of `libsvn_diff/lcs.c` (Apache Subversion 1.15.0, Apache License 2.0), based
//! on the Wu/Manber/Myers O(NP) algorithm.  The C code threads raw pointers through
//! circular linked lists; here positions and LCS nodes live in index arenas, which keeps the
//! algorithm (and therefore the chosen LCS among equal candidates) identical.

use crate::control::{Ctl, DiffError};

/// Index value used for a null pointer.
pub(crate) const NIL: usize = usize::MAX;

/// A token occurrence (`svn_diff__position_t`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Pos {
    /// Next position in the ring (`NIL` when unlinked).
    pub next: usize,
    /// Interned token id; negative values mark sentinels.
    pub token: i32,
    /// 1-based line offset within the file.
    pub offset: i64,
}

/// Appends a position to the arena and returns its index.
pub(crate) fn new_pos(arena: &mut Vec<Pos>, next: usize, token: i32, offset: i64) -> usize {
    arena.push(Pos {
        next,
        token,
        offset,
    });
    arena.len() - 1
}

/// One common run (`svn_diff__lcs_t`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct LcsNode {
    /// Start position in the first sequence.
    pub pos0: usize,
    /// Start position in the second sequence.
    pub pos1: usize,
    /// Number of common tokens; 0 marks EOF.
    pub length: i64,
    /// Next node (or next free node while on the freelist).
    pub next: usize,
    refcount: i32,
}

/// Node storage with the freelist recycling of the reference implementation.
#[derive(Debug)]
pub(crate) struct LcsArena {
    pub nodes: Vec<LcsNode>,
    free: usize,
}

impl LcsArena {
    pub(crate) fn new() -> Self {
        Self {
            nodes: Vec::new(),
            free: NIL,
        }
    }

    /// Adds a node outside the recycling machinery (refcount 1).
    pub(crate) fn push(&mut self, pos0: usize, pos1: usize, length: i64, next: usize) -> usize {
        self.alloc(LcsNode {
            pos0,
            pos1,
            length,
            next,
            refcount: 1,
        })
    }

    fn alloc(&mut self, node: LcsNode) -> usize {
        if self.free != NIL {
            let idx = self.free;
            self.free = self.nodes[idx].next;
            self.nodes[idx] = node;
            idx
        } else {
            self.nodes.push(node);
            self.nodes.len() - 1
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Snake {
    y: i64,
    lcs: usize,
    pos0: usize,
    pos1: usize,
}

const EMPTY_SNAKE: Snake = Snake {
    y: 0,
    lcs: NIL,
    pos0: NIL,
    pos1: NIL,
};

fn snake(fp: &mut [Snake], k: usize, token_counts: [&[u32]; 2], pos: &[Pos], arena: &mut LcsArena) {
    // Release the lcs chain previously stored at fp[k] if it was a dead end.
    let mut lcs = fp[k].lcs;
    while lcs != NIL {
        arena.nodes[lcs].refcount -= 1;
        if arena.nodes[lcs].refcount != 0 {
            break;
        }
        let previous = arena.nodes[lcs].next;
        arena.nodes[lcs].next = arena.free;
        arena.free = lcs;
        lcs = previous;
    }

    let (mut start0, mut start1, mut previous_lcs);
    if fp[k - 1].y >= fp[k + 1].y {
        start0 = fp[k - 1].pos0;
        start1 = pos[fp[k - 1].pos1].next;
        previous_lcs = fp[k - 1].lcs;
    } else {
        start0 = pos[fp[k + 1].pos0].next;
        start1 = fp[k + 1].pos1;
        previous_lcs = fp[k + 1].lcs;
    }
    if previous_lcs != NIL {
        arena.nodes[previous_lcs].refcount += 1;
    }

    let (mut p0, mut p1) = (start0, start1);
    loop {
        while pos[p0].token == pos[p1].token {
            p0 = pos[p0].next;
            p1 = pos[p1].next;
        }
        if p1 != start1 {
            previous_lcs = arena.alloc(LcsNode {
                pos0: start0,
                pos1: start1,
                length: pos[p1].offset - pos[start1].offset,
                next: previous_lcs,
                refcount: 1,
            });
            start0 = p0;
            start1 = p1;
        }
        // Skip tokens that occur in only one of the files.
        let t0 = pos[p0].token;
        let t1 = pos[p1].token;
        if t0 >= 0 && token_counts[1][t0 as usize] == 0 {
            p0 = pos[p0].next;
            start0 = p0;
        } else if t1 >= 0 && token_counts[0][t1 as usize] == 0 {
            p1 = pos[p1].next;
            start1 = p1;
        } else {
            break;
        }
    }
    fp[k] = Snake {
        y: pos[p1].offset,
        lcs: previous_lcs,
        pos0: p0,
        pos1: p1,
    };
}

fn reverse(arena: &mut LcsArena, mut lcs: usize) -> usize {
    let mut next = NIL;
    while lcs != NIL {
        let prev = arena.nodes[lcs].next;
        arena.nodes[lcs].next = next;
        next = lcs;
        lcs = prev;
    }
    next
}

/// Prepends a chunk of `lines` (> 0) common lines at the given offsets.
fn prepend_lcs(
    arena: &mut LcsArena,
    pos: &mut Vec<Pos>,
    lcs: usize,
    lines: i64,
    pos0_offset: i64,
    pos1_offset: i64,
) -> usize {
    let p0 = new_pos(pos, NIL, 0, pos0_offset);
    let p1 = new_pos(pos, NIL, 0, pos1_offset);
    arena.push(p0, p1, lines, lcs)
}

/// Counts occurrences per token over the ring whose tail is `loop_start`.
pub(crate) fn token_counts(pos: &[Pos], loop_start: usize, num_tokens: usize) -> Vec<u32> {
    let mut counts = vec![0u32; num_tokens];
    if loop_start != NIL {
        let mut cur = loop_start;
        loop {
            counts[pos[cur].token as usize] += 1;
            cur = pos[cur].next;
            if cur == loop_start {
                break;
            }
        }
    }
    counts
}

/// Computes the LCS (`svn_diff__lcs`) between two rings given by their tail positions
/// (`NIL` for an empty file).  Returns the head of the resulting chain, which always ends
/// with a zero-length EOF node.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lcs(
    pos: &mut Vec<Pos>,
    arena: &mut LcsArena,
    list1: usize,
    list2: usize,
    counts1: &[u32],
    counts2: &[u32],
    prefix_lines: i64,
    suffix_lines: i64,
    ctl: &mut Ctl<'_>,
) -> Result<usize, DiffError> {
    // EOF is always a sync point: tack on a zero-length node with sentinel positions.
    let off0 = if list1 != NIL {
        pos[list1].offset + suffix_lines + 1
    } else {
        prefix_lines + suffix_lines + 1
    };
    let off1 = if list2 != NIL {
        pos[list2].offset + suffix_lines + 1
    } else {
        prefix_lines + suffix_lines + 1
    };
    let eof0 = new_pos(pos, NIL, 0, off0);
    let eof1 = new_pos(pos, NIL, 0, off1);
    let mut head = arena.push(eof0, eof1, 0, NIL);

    if list1 == NIL || list2 == NIL {
        if suffix_lines != 0 {
            head = prepend_lcs(
                arena,
                pos,
                head,
                suffix_lines,
                off0 - suffix_lines,
                off1 - suffix_lines,
            );
        }
        if prefix_lines != 0 {
            head = prepend_lcs(arena, pos, head, prefix_lines, 1, 1);
        }
        return Ok(head);
    }

    let mut unique = [0i64; 2];
    for (c1, c2) in counts1.iter().zip(counts2) {
        if *c1 == 0 {
            unique[1] += i64::from(*c2);
        }
        if *c2 == 0 {
            unique[0] += i64::from(*c1);
        }
    }
    let first1 = pos[list1].next;
    let first2 = pos[list2].next;
    let len0 = pos[list1].offset - pos[first1].offset + 1 - unique[0];
    let len1 = pos[list2].offset - pos[first2].offset + 1 - unique[1];

    let mut fp = vec![EMPTY_SNAKE; (len0 + len1 + 3) as usize];
    let base = len1 + 1;
    let at = |k: i64| (base + k) as usize;

    let s0 = new_pos(pos, first1, -1, pos[list1].offset + 1);
    pos[list1].next = s0;
    let s1 = new_pos(pos, first2, -2, pos[list2].offset + 1);
    pos[list2].next = s1;

    let d = len0 - len1;
    fp[at(d - 1)].pos0 = pos[s0].next;
    fp[at(d - 1)].pos1 = s1;

    let counts = [counts1, counts2];
    let mut p: i64 = 0;
    loop {
        // For k < 0 insertions are free.
        let mut k = d.min(0) - p;
        while k < 0 {
            ctl.tick(1)?;
            snake(&mut fp, at(k), counts, pos, arena);
            k += 1;
        }
        // For k > 0 deletions are free.
        let mut k = d.max(0) + p;
        while k >= 0 {
            ctl.tick(1)?;
            snake(&mut fp, at(k), counts, pos, arena);
            k -= 1;
        }
        p += 1;
        if fp[at(0)].pos1 == s1 {
            break;
        }
    }
    // The loop above stops when the end state is reached, i.e. fp[0].pos1 is the sentinel.
    let chain = if suffix_lines != 0 {
        prepend_lcs(
            arena,
            pos,
            fp[at(0)].lcs,
            suffix_lines,
            off0 - suffix_lines,
            off1 - suffix_lines,
        )
    } else {
        fp[at(0)].lcs
    };
    arena.nodes[head].next = chain;
    let mut result = reverse(arena, head);

    pos[list1].next = pos[s0].next;
    pos[list2].next = pos[s1].next;

    if prefix_lines != 0 {
        result = prepend_lcs(arena, pos, result, prefix_lines, 1, 1);
    }
    Ok(result)
}
