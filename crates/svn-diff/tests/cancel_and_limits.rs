//! Cancellation, cost budget, binary-content handling and display-diff performance.

// Test code: panicking on a broken fixture is the intended failure mode.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::thread;
use std::time::{Duration, Instant};

use svn_diff::{
    DiffError, DiffOptions, DisplayOptions, MergeOptions, UnifiedOptions, diff,
    diff_as_binary_by_mime, merge3, unified,
};
use svn_types::CancelToken;

/// Deterministic pseudo-random document over a tiny vocabulary: worst case for the LCS.
fn low_vocab(lines: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    let mut out = Vec::with_capacity(lines * 3);
    for _ in 0..lines {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push(b'a' + ((state >> 33) % 4) as u8);
        out.push(b'\n');
    }
    out
}

#[test]
fn pathological_merge_is_cancelled_within_100ms() {
    let (base, mine, theirs) = (
        low_vocab(100_000, 1),
        low_vocab(100_000, 2),
        low_vocab(100_000, 3),
    );
    // Best of three: a scheduler hiccup on a loaded CI machine must not fail the test, while a
    // missing cancellation check would make every attempt slow.
    let best = (0..3)
        .map(|_| {
            let token = CancelToken::new();
            let canceller = token.clone();
            let cancelled_at = thread::spawn(move || {
                thread::sleep(Duration::from_millis(150));
                canceller.cancel();
                Instant::now()
            });
            let res = merge3(&base, &mine, &theirs, &MergeOptions::default(), &token);
            let returned_at = Instant::now();
            let cancelled_at = cancelled_at.join().unwrap();
            assert_eq!(res.unwrap_err(), DiffError::Cancelled);
            returned_at.saturating_duration_since(cancelled_at)
        })
        .min()
        .unwrap();
    assert!(best < Duration::from_millis(100), "cancel took {best:?}");
}

#[test]
fn pathological_unified_and_display_are_cancellable() {
    let (a, b) = (low_vocab(100_000, 7), low_vocab(100_000, 8));
    let token = CancelToken::new();
    token.cancel();
    assert_eq!(
        unified(&a, &b, &UnifiedOptions::default(), &token).unwrap_err(),
        DiffError::Cancelled
    );
    assert_eq!(
        diff(&a, &b, &DisplayOptions::default(), &token).unwrap_err(),
        DiffError::Cancelled
    );
}

#[test]
fn cost_budget_returns_too_complex() {
    let (a, b) = (low_vocab(20_000, 11), low_vocab(20_000, 12));
    let opts = UnifiedOptions {
        diff: DiffOptions {
            max_cost: Some(1_000_000),
            ..DiffOptions::default()
        },
        ..UnifiedOptions::default()
    };
    let started = Instant::now();
    let err = unified(&a, &b, &opts, &CancelToken::new()).unwrap_err();
    assert_eq!(err, DiffError::TooComplex);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn nul_content_without_mime_type_is_text() {
    assert!(!diff_as_binary_by_mime(None));
    let cancel = CancelToken::new();
    let (base, mine, theirs) = (b"a\0x\nb\nc\n", b"a\0y\nb\nc\n", b"a\0x\nb\nC\n");
    let out = unified(base, mine, &UnifiedOptions::default(), &cancel).unwrap();
    assert!(String::from_utf8_lossy(&out).contains("@@"));
    let merged = merge3(base, mine, theirs, &MergeOptions::default(), &cancel).unwrap();
    assert_eq!(merged.text, b"a\0y\nb\nC\n");
    assert!(merged.conflicts.is_empty());
}

#[test]
fn display_diff_alternating_lines_stays_fast() {
    // a/b alternating with a one-line shift: pathological for histogram/Myers diffs.
    let alt = |shift: usize, n: usize| -> Vec<u8> {
        (0..n)
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
    let (a, b) = (alt(0, 20_000), alt(1, 20_000));
    let best = (0..5)
        .map(|_| {
            let t = Instant::now();
            diff(&a, &b, &DisplayOptions::default(), &CancelToken::new()).unwrap();
            t.elapsed()
        })
        .min()
        .unwrap();
    assert!(best < Duration::from_millis(50), "took {best:?}");
}
