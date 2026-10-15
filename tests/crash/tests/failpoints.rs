//! Named failpoints of `svn-wc` / `svn-client` turned into a `SIGKILL` of the worker: for every
//! operation and failpoint, an uninterrupted run counts the hits; the worker is then killed at
//! the first hit, the last hit and random hits in between (`update.after_txn_before_queue` at
//! hit k is "mid-update with local edits" after k files; `commit.after_merge_before_finalize`
//! is "after MERGE").
//!
//! Every failpoint gets at least `CRASH_FP_ITERS` kills, spread over the operations that reach
//! it. An operation that hits a failpoint fewer times than its share is killed at each hit, then
//! again at random hits (thread timing differs from run to run) until the share is used up.
//!
//! `cargo test -p opensvn-crash --features failpoints --test failpoints -- --ignored --nocapture`
//! Knobs: `CRASH_FP_ITERS` (kills per reachable failpoint, default 50), plus those of the sigkill
//! suite.
#![cfg(all(target_os = "linux", feature = "failpoints"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::Path;

use opensvn_crash::harness::{FAILPOINTS, Fixture, Kill, Op, Report, env_count};
use opensvn_crash::rng::Rng;

#[test]
#[ignore = "needs apache2 + mod_dav_svn and svn; run by the nightly workflow"]
fn failpoint_suite() {
    let (mut rng, seed) = Rng::from_env();
    eprintln!("CRASH_SEED={seed}");
    let worker = Path::new(env!("CARGO_BIN_EXE_crash-worker"));
    let Some(fx) = Fixture::new(worker, env_count("CRASH_FILES", 200), &mut rng).expect("fixture")
    else {
        return;
    };
    let iters = env_count("CRASH_FP_ITERS", 50) as u64;
    let mut report = Report {
        seed,
        ..Report::default()
    };
    for fp in FAILPOINTS {
        let mut reach = Vec::new();
        for op in Op::ALL {
            let n = fx.count_hits(op, fp).expect("count hits");
            report.coverage.insert((op, (*fp).to_owned()), n);
            if n > 0 {
                reach.push((op, n));
            }
        }
        for (i, (op, n)) in reach.iter().copied().enumerate() {
            // This operation's share of the kills; the first ones take the remainder.
            let share =
                iters / reach.len() as u64 + u64::from((i as u64) < iters % reach.len() as u64);
            for hit in kill_hits(&mut rng, n, share.max(1)) {
                let kill = Kill::Failpoint {
                    name: (*fp).to_owned(),
                    hit,
                };
                let rec = fx.case(op, &kill, None);
                if let Some(f) = &rec.failure {
                    eprintln!("FAIL case {} {} {}: {f}", rec.id, op.name(), rec.kill);
                }
                report.cases.push(rec);
            }
        }
    }
    let title = "Crash suite: named failpoints";
    let path = report.write("failpoints", title).expect("report");
    eprintln!("{}\nreport: {}", report.markdown(title), path.display());
    assert!(
        report.failures().is_empty(),
        "{} of {} failpoint cases failed (seed {seed}); see {}",
        report.failures().len(),
        report.cases.len(),
        path.display()
    );
}

/// `want` hits to kill at among `1..=n`: the first and the last hit, distinct random ones, then
/// (when `n < want`) random repeats.
fn kill_hits(rng: &mut Rng, n: u64, want: u64) -> Vec<u64> {
    let mut distinct = BTreeSet::from([1, n]);
    let mut guard = 0;
    while (distinct.len() as u64) < want.min(n) && guard < 10_000 {
        distinct.insert(1 + rng.below(n));
        guard += 1;
    }
    let mut hits: Vec<u64> = distinct.into_iter().take(want as usize).collect();
    while (hits.len() as u64) < want {
        hits.push(1 + rng.below(n));
    }
    hits
}
