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
// Modified by OpenSVN: ported from token.c, diff.c and diff3.c onto index arenas; in-memory buffers; cancellation/budget checks.

//! Two-way diff (`svn_diff_diff_2`) and three-way diff (`svn_diff_diff3_2`) producing chunk
//! lists with libsvn_diff semantics.
//!
//! Ports of `libsvn_diff/{token.c,diff.c,diff3.c}` from Apache Subversion 1.15.0
//! (Apache License 2.0).  Line numbers in chunks are 0-based like in the C structures.

use std::borrow::Cow;
use std::collections::HashMap;

use crate::affix::{Affix, find_affix};
use crate::control::{Ctl, DiffError};
use crate::lcs::{LcsArena, NIL, Pos, lcs, new_pos, token_counts};
use crate::tokens::{DiffOptions, normalize, split_lines};

/// Kind of a diff chunk (`svn_diff__type_e`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChunkKind {
    /// Identical in all inputs.
    Common,
    /// Changed only in `modified`.
    DiffModified,
    /// Changed only in `latest`.
    DiffLatest,
    /// Changed identically in `modified` and `latest`.
    DiffCommon,
    /// Changed differently in `modified` and `latest`.
    Conflict,
}

/// A region of the diff (`svn_diff_t`).  Starts are 0-based line indices.
#[derive(Debug, Clone)]
pub(crate) struct Chunk {
    pub kind: ChunkKind,
    pub original_start: i64,
    pub original_length: i64,
    pub modified_start: i64,
    pub modified_length: i64,
    pub latest_start: i64,
    pub latest_length: i64,
    /// Finer-grained breakdown of a conflict (`resolved_diff`).
    pub resolved: Vec<Chunk>,
}

impl Chunk {
    fn new(kind: ChunkKind, o: (i64, i64), m: (i64, i64), l: (i64, i64)) -> Self {
        Self {
            kind,
            original_start: o.0,
            original_length: o.1,
            modified_start: m.0,
            modified_length: m.1,
            latest_start: l.0,
            latest_length: l.1,
            resolved: Vec::new(),
        }
    }
}

/// Tokenized inputs ready for the LCS step.
struct Prepared {
    pos: Vec<Pos>,
    /// Ring tails per input (`NIL` when the input has no middle section).
    tails: Vec<usize>,
    counts: Vec<Vec<u32>>,
    num_tokens: usize,
    affix: Affix,
}

fn prepare(files: &[&[u8]], opts: &DiffOptions) -> Prepared {
    let affix = find_affix(files);
    let mut interner: HashMap<Cow<'_, [u8]>, i32> = HashMap::new();
    let mut pos: Vec<Pos> = Vec::new();
    let mut tails = Vec::with_capacity(files.len());
    for file in files {
        let end = file.len() - affix.suffix_len;
        let middle = file.get(affix.prefix_len..end).unwrap_or(&[]);
        let mut offset = affix.prefix_lines;
        let mut first = NIL;
        let mut last = NIL;
        for line in split_lines(middle) {
            offset += 1;
            let next_id = interner.len() as i32;
            let id = *interner.entry(normalize(line, opts)).or_insert(next_id);
            let idx = new_pos(&mut pos, NIL, id, offset);
            if first == NIL {
                first = idx;
            } else {
                pos[last].next = idx;
            }
            last = idx;
        }
        if last != NIL {
            pos[last].next = first; // close the ring
        }
        tails.push(last);
    }
    let num_tokens = interner.len();
    let counts = tails
        .iter()
        .map(|&t| token_counts(&pos, t, num_tokens))
        .collect();
    Prepared {
        pos,
        tails,
        counts,
        num_tokens,
        affix,
    }
}

/// `svn_diff__diff`: converts an LCS chain into diff chunks.
fn diff_from_lcs(
    arena: &LcsArena,
    pos: &[Pos],
    mut lcs_idx: usize,
    mut original_start: i64,
    mut modified_start: i64,
    want_common: bool,
) -> Vec<Chunk> {
    let mut out = Vec::new();
    loop {
        let node = arena.nodes[lcs_idx];
        let o_off = pos[node.pos0].offset;
        let m_off = pos[node.pos1].offset;
        if original_start < o_off || modified_start < m_off {
            out.push(Chunk::new(
                ChunkKind::DiffModified,
                (original_start - 1, o_off - original_start),
                (modified_start - 1, m_off - modified_start),
                (0, 0),
            ));
        }
        if node.length == 0 {
            break;
        }
        original_start = o_off;
        modified_start = m_off;
        if want_common {
            out.push(Chunk::new(
                ChunkKind::Common,
                (original_start - 1, node.length),
                (modified_start - 1, node.length),
                (0, 0),
            ));
        }
        original_start += node.length;
        modified_start += node.length;
        lcs_idx = node.next;
    }
    out
}

/// Computes the two-way diff between `original` and `modified`.
pub(crate) fn diff2(
    original: &[u8],
    modified: &[u8],
    opts: &DiffOptions,
    ctl: &mut Ctl<'_>,
) -> Result<Vec<Chunk>, DiffError> {
    let mut p = prepare(&[original, modified], opts);
    let mut arena = LcsArena::new();
    let head = lcs(
        &mut p.pos,
        &mut arena,
        p.tails[0],
        p.tails[1],
        &p.counts[0],
        &p.counts[1],
        p.affix.prefix_lines,
        p.affix.suffix_lines,
        ctl,
    )?;
    Ok(diff_from_lcs(&arena, &p.pos, head, 1, 1, true))
}

/// `svn_diff__resolve_conflict`: refines a conflicting hunk by diffing the two changed
/// regions against each other.  Rewires the position rings, like the C code.
fn resolve_conflict(
    hunk: &mut Chunk,
    pos: &mut Vec<Pos>,
    list1: &mut usize,
    list2: &mut usize,
    counts_len: usize,
    ctl: &mut Ctl<'_>,
) -> Result<(), DiffError> {
    let mut modified_start = hunk.modified_start + 1;
    let mut latest_start = hunk.latest_start + 1;
    let mut modified_length = hunk.modified_length;
    let mut latest_length = hunk.latest_length;

    let mut start0 = *list1;
    let mut start1 = *list2;
    while pos[start0].offset < modified_start {
        start0 = pos[start0].next;
    }
    while pos[start1].offset < latest_start {
        start1 = pos[start1].next;
    }
    let (mut p0, mut p1) = (start0, start1);

    let mut common_length = modified_length.min(latest_length);
    while common_length > 0 && pos[p0].token == pos[p1].token {
        p0 = pos[p0].next;
        p1 = pos[p1].next;
        common_length -= 1;
    }

    if common_length == 0 && modified_length == latest_length {
        hunk.kind = ChunkKind::DiffCommon;
        hunk.resolved = Vec::new();
        *list1 = p0;
        *list2 = p1;
        return Ok(());
    }
    hunk.kind = ChunkKind::Conflict;

    let mut arena = LcsArena::new();
    // How much of the two sequences was actually the same at the start.
    common_length = modified_length.min(latest_length) - common_length;
    let mut head = NIL;
    let mut tail_link = NIL; // node whose `next` receives the sub-LCS; NIL = head itself
    if common_length > 0 {
        head = arena.push(start0, start1, common_length, NIL);
        tail_link = head;
    }
    modified_length -= common_length;
    latest_length -= common_length;

    modified_start = pos[start0].offset;
    latest_start = pos[start1].offset;

    let (rest0, rest1) = (p0, p1);
    // Build a fresh ring for each remaining region.
    let ring0 = if modified_length == 0 {
        *list1 = p0;
        NIL
    } else {
        let mut last = p0;
        modified_length -= 1;
        while modified_length > 0 {
            last = pos[last].next;
            modified_length -= 1;
        }
        *list1 = pos[last].next;
        pos[last].next = rest0;
        last
    };
    let ring1 = if latest_length == 0 {
        *list2 = p1;
        NIL
    } else {
        let mut last = p1;
        latest_length -= 1;
        while latest_length > 0 {
            last = pos[last].next;
            latest_length -= 1;
        }
        *list2 = pos[last].next;
        pos[last].next = rest1;
        last
    };

    let c0 = token_counts(pos, ring0, counts_len);
    let c1 = token_counts(pos, ring1, counts_len);
    let sub = lcs(pos, &mut arena, ring0, ring1, &c0, &c1, 0, 0, ctl)?;
    if tail_link == NIL {
        head = sub;
    } else {
        arena.nodes[tail_link].next = sub;
    }

    // Fix up the first sub-LCS node when it starts at offset 1: this is how the reference
    // handles an empty sequence (EOF node) and it also applies to a real run at offset 1.
    if pos[arena.nodes[sub].pos0].offset == 1 {
        arena.nodes[sub].pos0 = *list1;
    }
    if pos[arena.nodes[sub].pos1].offset == 1 {
        arena.nodes[sub].pos1 = *list2;
    }

    let mut resolved = Vec::new();
    let mut cur = head;
    loop {
        let node = arena.nodes[cur];
        let o0 = pos[node.pos0].offset;
        let o1 = pos[node.pos1].offset;
        if modified_start < o0 || latest_start < o1 {
            resolved.push(Chunk::new(
                ChunkKind::Conflict,
                (hunk.original_start, hunk.original_length),
                (modified_start - 1, o0 - modified_start),
                (latest_start - 1, o1 - latest_start),
            ));
        }
        if node.length == 0 {
            break;
        }
        modified_start = o0;
        latest_start = o1;
        resolved.push(Chunk::new(
            ChunkKind::DiffCommon,
            (hunk.original_start, hunk.original_length),
            (modified_start - 1, node.length),
            (latest_start - 1, node.length),
        ));
        modified_start += node.length;
        latest_start += node.length;
        cur = node.next;
    }
    hunk.resolved = resolved;
    Ok(())
}

/// Computes the three-way diff of `original`, `modified` (mine) and `latest` (theirs).
pub(crate) fn diff3(
    original: &[u8],
    modified: &[u8],
    latest: &[u8],
    opts: &DiffOptions,
    ctl: &mut Ctl<'_>,
) -> Result<Vec<Chunk>, DiffError> {
    let mut p = prepare(&[original, modified, latest], opts);
    let prefix_lines = p.affix.prefix_lines;
    let suffix_lines = p.affix.suffix_lines;
    let (mut arena_om, mut arena_ol) = (LcsArena::new(), LcsArena::new());
    let mut lcs_om = lcs(
        &mut p.pos,
        &mut arena_om,
        p.tails[0],
        p.tails[1],
        &p.counts[0],
        &p.counts[1],
        prefix_lines,
        suffix_lines,
        ctl,
    )?;
    let mut lcs_ol = lcs(
        &mut p.pos,
        &mut arena_ol,
        p.tails[0],
        p.tails[2],
        &p.counts[0],
        &p.counts[2],
        prefix_lines,
        suffix_lines,
        ctl,
    )?;

    let mut out: Vec<Chunk> = Vec::new();
    let mut original_start: i64 = 1;
    let mut modified_start: i64 = 1;
    let mut latest_start: i64 = 1;

    // Point the position lists at the ring heads so conflict detection can walk them.
    let mut list = [NIL; 2];
    for (slot, tail) in [(0usize, p.tails[1]), (1usize, p.tails[2])] {
        if tail != NIL {
            let head = p.pos[tail].next;
            let off = p.pos[tail].offset + 1;
            let s = new_pos(&mut p.pos, head, 0, off);
            p.pos[tail].next = s;
            list[slot] = head;
        } else {
            list[slot] = new_pos(&mut p.pos, NIL, 0, prefix_lines + 1);
        }
    }

    macro_rules! om {
        () => {
            arena_om.nodes[lcs_om]
        };
    }
    macro_rules! ol {
        () => {
            arena_ol.nodes[lcs_ol]
        };
    }
    macro_rules! off {
        ($p:expr) => {
            p.pos[$p].offset
        };
    }

    loop {
        ctl.tick(1)?;
        // Find the sync points.
        let original_sync;
        loop {
            if off!(om!().pos0) > off!(ol!().pos0) {
                let sync = off!(om!().pos0);
                while off!(ol!().pos0) + ol!().length < sync {
                    lcs_ol = ol!().next;
                }
                // At EOF, skip an lcs segment that does not reach EOF.
                if om!().length == 0
                    && ol!().length > 0
                    && off!(ol!().pos0) + ol!().length == sync
                    && off!(ol!().pos1) + ol!().length != off!(arena_ol.nodes[ol!().next].pos1)
                {
                    lcs_ol = ol!().next;
                }
                if off!(ol!().pos0) <= sync {
                    original_sync = sync;
                    break;
                }
            } else {
                let sync = off!(ol!().pos0);
                while off!(om!().pos0) + om!().length < sync {
                    lcs_om = om!().next;
                }
                if ol!().length == 0
                    && om!().length > 0
                    && off!(om!().pos0) + om!().length == sync
                    && off!(om!().pos1) + om!().length != off!(arena_om.nodes[om!().next].pos1)
                {
                    lcs_om = om!().next;
                }
                if off!(om!().pos0) <= sync {
                    original_sync = sync;
                    break;
                }
            }
        }

        let modified_sync = off!(om!().pos1) + (original_sync - off!(om!().pos0));
        let latest_sync = off!(ol!().pos1) + (original_sync - off!(ol!().pos0));

        let is_modified =
            off!(om!().pos0) - original_start > 0 || off!(om!().pos1) - modified_start > 0;
        let is_latest =
            off!(ol!().pos0) - original_start > 0 || off!(ol!().pos1) - latest_start > 0;

        if is_modified || is_latest {
            let mut chunk = Chunk::new(
                ChunkKind::DiffModified,
                (original_start - 1, original_sync - original_start),
                (modified_start - 1, modified_sync - modified_start),
                (latest_start - 1, latest_sync - latest_start),
            );
            if is_modified && is_latest {
                let [l1, l2] = &mut list;
                resolve_conflict(&mut chunk, &mut p.pos, l1, l2, p.num_tokens, ctl)?;
            } else if is_latest {
                chunk.kind = ChunkKind::DiffLatest;
            }
            out.push(chunk);
        }

        // Detect EOF.
        if om!().length == 0 || ol!().length == 0 {
            break;
        }

        let m_len = om!().length - (original_sync - off!(om!().pos0));
        let l_len = ol!().length - (original_sync - off!(ol!().pos0));
        let common_length = m_len.min(l_len);
        if common_length > 0 {
            out.push(Chunk::new(
                ChunkKind::Common,
                (original_sync - 1, common_length),
                (modified_sync - 1, common_length),
                (latest_sync - 1, common_length),
            ));
        }

        original_start = original_sync + common_length;
        modified_start = modified_sync + common_length;
        latest_start = latest_sync + common_length;

        // Remember the last lcs start positions for conflict detection.
        if off!(list[0]) < off!(om!().pos1) {
            list[0] = om!().pos1;
        }
        if off!(list[1]) < off!(ol!().pos1) {
            list[1] = ol!().pos1;
        }

        while original_start >= off!(om!().pos0) + om!().length && om!().length > 0 {
            lcs_om = om!().next;
        }
        while original_start >= off!(ol!().pos0) + ol!().length && ol!().length > 0 {
            lcs_ol = ol!().next;
        }
    }
    Ok(out)
}
