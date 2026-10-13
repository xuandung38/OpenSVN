//! Timing harness for the documented targets (diff 20k lines, merge3 100k lines).
//! Run with `cargo bench -p svn-diff`; prints wall-clock times.

// Test code: panicking on a broken fixture is the intended failure mode.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::time::Instant;

use svn_diff::{DisplayOptions, MergeOptions, UnifiedOptions, diff, merge3, unified};
use svn_types::CancelToken;

fn make(lines: usize, step: usize, tag: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..lines {
        if i % step == 0 {
            out.extend_from_slice(format!("{tag} changed line {i}\n").as_bytes());
        } else {
            out.extend_from_slice(format!("line number {i} of the file\n").as_bytes());
        }
    }
    out
}

/// Best-of-N wall time; the minimum filters out scheduler noise on busy machines.
fn best<T>(runs: usize, mut f: impl FnMut() -> T) -> (std::time::Duration, T) {
    let mut best = None;
    let mut last = None;
    for _ in 0..runs {
        let t = Instant::now();
        let v = f();
        let d = t.elapsed();
        if best.is_none_or(|b| d < b) {
            best = Some(d);
        }
        last = Some(v);
    }
    (best.unwrap_or_default(), last.unwrap_or_else(&mut f))
}

fn main() {
    let base20 = make(20_000, usize::MAX, "x");
    let new20 = make(20_000, 97, "m");
    let (t, h) = best(7, || {
        diff(
            &base20,
            &new20,
            &DisplayOptions::default(),
            &CancelToken::new(),
        )
    });
    println!(
        "display diff 20k lines: {t:?} ({} hunks)  target < 50ms",
        h.as_ref().map_or(0, Vec::len)
    );
    let (t, u) = best(7, || {
        unified(
            &base20,
            &new20,
            &UnifiedOptions::default(),
            &CancelToken::new(),
        )
    });
    println!(
        "unified 20k lines: {t:?} ({} bytes)",
        u.as_ref().map_or(0, Vec::len)
    );

    let alt = |shift: usize| -> Vec<u8> {
        (0..20_000usize)
            .flat_map(|i| {
                if (i + shift).is_multiple_of(2) {
                    b"a\n"
                } else {
                    b"b\n"
                }
            })
            .copied()
            .collect()
    };
    let (a, b) = (alt(0), alt(1));
    let (t, h) = best(7, || {
        diff(&a, &b, &DisplayOptions::default(), &CancelToken::new())
    });
    println!(
        "display diff 20k alternating lines: {t:?} ({} hunks)  target < 50ms",
        h.as_ref().map_or(0, Vec::len)
    );

    let base = make(100_000, usize::MAX, "x");
    let mine = make(100_000, 997, "m");
    let theirs = make(100_000, 1009, "t");
    let (t, r) = best(7, || {
        merge3(
            &base,
            &mine,
            &theirs,
            &MergeOptions::default(),
            &CancelToken::new(),
        )
    });
    println!(
        "merge3 100k lines: {t:?} ({} conflicts)  target < 300ms",
        r.as_ref().map_or(0, |r| r.conflicts.len())
    );
}
