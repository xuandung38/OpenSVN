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
// Modified by OpenSVN: ported from diff_file.c prefix/suffix scanning onto in-memory buffers.

//! Identical prefix/suffix detection shared by every input of a diff.
//!
//! Port of `find_identical_prefix` / `find_identical_suffix` from
//! `libsvn_diff/diff_file.c` (Apache Subversion 1.15.0, Apache License 2.0), on in-memory
//! buffers.  Skipping the common prefix/suffix changes which of several equally good
//! LCS solutions is chosen, so it is part of the observable behaviour.

/// Number of identical suffix lines left to the LCS step (`SUFFIX_LINES_TO_KEEP`).
const SUFFIX_LINES_TO_KEEP: i32 = 50;

/// Result of the prefix/suffix scan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Affix {
    /// Bytes of identical prefix (ends right after an EOL).
    pub prefix_len: usize,
    /// Lines in the identical prefix.
    pub prefix_lines: i64,
    /// Bytes of identical suffix (starts at a line start).
    pub suffix_len: usize,
    /// Lines in the identical suffix.
    pub suffix_lines: i64,
}

fn at(f: &[u8], i: isize) -> u8 {
    usize::try_from(i)
        .ok()
        .and_then(|u| f.get(u))
        .copied()
        .unwrap_or(0)
}

/// Returns (prefix end offset, prefix lines, reached_one_eof).
fn find_identical_prefix(files: &[&[u8]]) -> (usize, i64, bool) {
    let Some(first) = files.first() else {
        return (0, 0, false);
    };
    let same = |p: isize| files.iter().all(|f| at(f, p) == at(first, p));
    let mut pos: isize = 0;
    let mut lines: i64 = 0;
    let mut had_cr = false;
    let mut reached = false;
    let mut is_match = same(0);
    while is_match {
        let b = at(first, pos);
        if b == b'\r' {
            lines += 1;
            had_cr = true;
        } else if b == b'\n' && !had_cr {
            lines += 1;
        } else {
            had_cr = false;
        }
        pos += 1;
        reached = files.iter().any(|f| pos as usize == f.len());
        if reached {
            break;
        }
        is_match = same(pos);
    }
    if had_cr {
        // Ended between "\r" and "\n" in one file but not in another: back up the line.
        let nonmatching_nl = files
            .iter()
            .any(|f| (pos as usize) < f.len() && at(f, pos) == b'\n');
        if nonmatching_nl {
            lines -= 1;
            pos -= 1;
        }
    }
    // Point at the last identical byte, then back up to the last EOL sequence.
    pos -= 1;
    while pos >= 0 && at(first, pos) != b'\n' && at(first, pos) != b'\r' {
        pos -= 1;
    }
    pos += 1;
    (pos as usize, lines, reached)
}

/// Returns (suffix length in bytes, suffix lines).
fn find_identical_suffix(files: &[&[u8]], prefix_end: usize) -> (usize, i64) {
    let Some(first) = files.first() else {
        return (0, 0);
    };
    let mut idx: Vec<isize> = files.iter().map(|f| f.len() as isize - 1).collect();
    let min_size = files.iter().map(|f| f.len()).min().unwrap_or(0);
    let min_off0 = (prefix_end + (first.len() - min_size)) as isize;

    let same = |idx: &[isize]| {
        let b0 = at(first, idx.first().copied().unwrap_or(-1));
        files.iter().zip(idx).all(|(f, &i)| at(f, i) == b0)
    };
    let at_bof = |idx: &[isize]| idx.iter().any(|&i| i < 0);
    let at_eof = |idx: &[isize]| idx.iter().zip(files).any(|(&i, f)| i as usize == f.len());

    let mut lines: i64 = 0;
    let mut is_match = same(&idx);
    let b = at(first, idx[0]);
    if is_match && b != b'\r' && b != b'\n' {
        lines += 1;
    }
    let mut had_nl = false;
    while is_match {
        let b = at(first, idx[0]);
        if b == b'\n' {
            lines += 1;
            had_nl = true;
        } else if b == b'\r' && !had_nl {
            lines += 1;
        } else {
            had_nl = false;
        }
        idx.iter_mut().for_each(|i| *i -= 1);
        if idx[0] == min_off0 || at_bof(&idx) {
            break;
        }
        is_match = same(&idx);
    }
    idx.iter_mut().for_each(|i| *i += 1);

    // Complete the current line, then keep SUFFIX_LINES_TO_KEEP more lines.
    let mut keep = SUFFIX_LINES_TO_KEEP;
    loop {
        let mut had_cr = false;
        while !at_eof(&idx) && at(first, idx[0]) != b'\n' && at(first, idx[0]) != b'\r' {
            idx.iter_mut().for_each(|i| *i += 1);
        }
        if !at_eof(&idx) && at(first, idx[0]) == b'\r' {
            lines -= 1;
            had_cr = true;
            idx.iter_mut().for_each(|i| *i += 1);
        }
        if !at_eof(&idx) && at(first, idx[0]) == b'\n' {
            if !had_cr {
                lines -= 1;
            }
            idx.iter_mut().for_each(|i| *i += 1);
        }
        let cont = !at_eof(&idx) && {
            let c = keep != 0;
            keep -= 1;
            c
        };
        if !cont {
            break;
        }
    }
    if at_eof(&idx) {
        return (0, 0);
    }
    (first.len() - idx[0] as usize, lines)
}

/// Computes the identical prefix/suffix of all `files` (2 or 3 inputs).
pub(crate) fn find_affix(files: &[&[u8]]) -> Affix {
    if files.iter().any(|f| f.is_empty()) {
        return Affix::default();
    }
    let (prefix_len, prefix_lines, reached_eof) = find_identical_prefix(files);
    let mut affix = Affix {
        prefix_len,
        prefix_lines,
        ..Affix::default()
    };
    if !reached_eof {
        let (suffix_len, suffix_lines) = find_identical_suffix(files, prefix_len);
        affix.suffix_len = suffix_len;
        affix.suffix_lines = suffix_lines;
    }
    affix
}
