//! Property tests for merge identities, display-diff validity and unified round-trips.

// Test code: panicking on a broken fixture is the intended failure mode.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use svn_diff::{
    ConflictStyle, DiffOptions, DisplayOptions, IgnoreSpace, LineKind, MergeOptions,
    UnifiedOptions, diff, merge3, split_lines, unified,
};

/// Small vocabulary and mixed EOLs maximize repeated lines and ambiguous alignments.
fn doc() -> impl Strategy<Value = Vec<u8>> {
    let eol = prop_oneof![Just(&b"\n"[..]), Just(&b"\r\n"[..]), Just(&b"\r"[..])];
    let line = (0u8..5, eol).prop_map(|(w, e)| {
        let mut l = vec![b'a' + w];
        l.extend_from_slice(e);
        l
    });
    (prop::collection::vec(line, 0..40), any::<bool>()).prop_map(|(ls, strip)| {
        let mut d: Vec<u8> = ls.concat();
        if strip {
            while matches!(d.last(), Some(b'\n' | b'\r')) {
                d.pop();
            }
        }
        d
    })
}

fn any_style() -> impl Strategy<Value = ConflictStyle> {
    prop_oneof![
        Just(ConflictStyle::ModifiedOriginalLatest),
        Just(ConflictStyle::ModifiedLatest),
        Just(ConflictStyle::ResolvedModifiedLatest),
        Just(ConflictStyle::Modified),
        Just(ConflictStyle::Latest),
        Just(ConflictStyle::OnlyConflicts),
    ]
}

fn opts() -> MergeOptions {
    MergeOptions::default()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn merge_with_unchanged_theirs_is_mine(base in doc(), x in doc()) {
        let r = merge3(&base, &x, &base, &opts());
        prop_assert_eq!(r.text, x);
        prop_assert!(r.conflicts.is_empty());
    }

    #[test]
    fn merge_with_unchanged_mine_is_theirs(base in doc(), y in doc()) {
        let r = merge3(&base, &base, &y, &opts());
        prop_assert_eq!(r.text, y);
        prop_assert!(r.conflicts.is_empty());
    }

    #[test]
    fn identical_edits_never_conflict(base in doc(), x in doc()) {
        let r = merge3(&base, &x, &x, &opts());
        prop_assert_eq!(r.text, x);
        prop_assert!(r.conflicts.is_empty());
    }

    #[test]
    fn merge_never_panics(base in doc(), m in doc(), t in doc(), style in any_style()) {
        let o = MergeOptions { style, ..MergeOptions::default() };
        let r = merge3(&base, &m, &t, &o);
        if r.conflicts.is_empty() && style != ConflictStyle::OnlyConflicts {
            // A conflict-free merge has no marker lines unless the inputs already had them.
            prop_assert!(!r.text.starts_with(b"<<<<<<<"));
        }
    }

    #[test]
    fn display_hunks_rebuild_new(a in doc(), b in doc(), ctx in 0usize..5) {
        let hunks = diff(&a, &b, &DisplayOptions { diff: DiffOptions::default(), context: ctx });
        let a_lines = split_lines(&a);
        let mut rebuilt: Vec<u8> = Vec::new();
        let mut at = 0usize;
        for h in &hunks {
            prop_assert!(h.old_start >= at);
            for l in &a_lines[at..h.old_start] { rebuilt.extend_from_slice(l); }
            for line in &h.lines {
                if line.kind != LineKind::Removed { rebuilt.extend_from_slice(&line.text); }
            }
            at = h.old_start + h.old_len;
        }
        for l in &a_lines[at..] { rebuilt.extend_from_slice(l); }
        prop_assert_eq!(rebuilt, b);
    }

    #[test]
    fn display_ignoring_space_is_empty_for_space_only_changes(a in doc()) {
        let spaced: Vec<u8> = split_lines(&a).iter().flat_map(|l| {
            let mut v = vec![b' ', b'\t'];
            v.extend_from_slice(l);
            v
        }).collect();
        let o = DisplayOptions {
            diff: DiffOptions { ignore_space: IgnoreSpace::All, ignore_eol_style: false },
            context: 3,
        };
        prop_assert!(diff(&a, &spaced, &o).is_empty());
    }

    #[test]
    fn unified_output_applies_cleanly(a in doc(), b in doc()) {
        let out = unified(&a, &b, &UnifiedOptions::default());
        if a == b {
            prop_assert!(out.is_empty());
        } else {
            prop_assert_eq!(apply_unified(&a, &out), b);
        }
    }
}

/// Minimal unified-diff applier: hunks are replayed against the original lines.
fn apply_unified(orig: &[u8], patch: &[u8]) -> Vec<u8> {
    let orig_lines = split_lines(orig);
    let plines = split_lines(patch);
    let mut out: Vec<u8> = Vec::new();
    let mut at = 0usize;
    let mut i = 2; // skip ---/+++
    let mut last_kind = b' ';
    while i < plines.len() {
        let l = plines[i];
        i += 1;
        if l.starts_with(b"@@") {
            let hdr = String::from_utf8_lossy(l).into_owned();
            let old = hdr
                .split_whitespace()
                .nth(1)
                .unwrap_or("-0")
                .trim_start_matches('-')
                .to_owned();
            let mut p = old.split(',');
            let start: usize = p.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let len: usize = p.next().and_then(|s| s.parse().ok()).unwrap_or(1);
            let start0 = if len == 0 { start } else { start - 1 };
            for ol in &orig_lines[at..start0] {
                out.extend_from_slice(ol);
            }
            at = start0;
        } else if l.starts_with(b"\\") {
            // "No newline at end of file" applies to the previous line: strip its EOL.
            if last_kind != b'-' {
                while matches!(out.last(), Some(b'\n' | b'\r')) {
                    out.pop();
                }
            }
        } else {
            let (k, text) = (l[0], &l[1..]);
            last_kind = k;
            match k {
                b' ' => {
                    out.extend_from_slice(text);
                    at += 1;
                }
                b'-' => {
                    at += 1;
                }
                b'+' => out.extend_from_slice(text),
                _ => panic!("bad patch line"),
            }
        }
    }
    for ol in &orig_lines[at..] {
        out.extend_from_slice(ol);
    }
    out
}
