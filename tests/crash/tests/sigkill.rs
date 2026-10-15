//! Random `SIGKILL` from outside: each operation is killed at a uniformly random moment of its
//! measured duration (and a little after, so some runs complete), then verified.
//!
//! `cargo test -p opensvn-crash --test sigkill -- --ignored --nocapture`
//! Knobs: `CRASH_RANDOM_ITERS` (per operation, default 6), `CRASH_FILES` (default 200),
//! `CRASH_SEED`, `CRASH_REPORT_DIR`, `CRASH_KEEP_FAILED`.
#![cfg(target_os = "linux")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::Path;

use opensvn_crash::harness::{Fixture, Kill, Op, Report, env_count};
use opensvn_crash::rng::Rng;

#[test]
#[ignore = "needs apache2 + mod_dav_svn and svn; run by the nightly workflow"]
fn random_sigkill_suite() {
    let (mut rng, seed) = Rng::from_env();
    eprintln!("CRASH_SEED={seed}");
    let worker = Path::new(env!("CARGO_BIN_EXE_crash-worker"));
    let Some(fx) = Fixture::new(worker, env_count("CRASH_FILES", 200), &mut rng).expect("fixture")
    else {
        return;
    };
    let iters = env_count("CRASH_RANDOM_ITERS", 6);
    let mut report = Report {
        seed,
        ..Report::default()
    };
    for op in Op::ALL {
        let rec = fx.control(op);
        if let Some(f) = &rec.failure {
            eprintln!("FAIL control {}: {f}", op.name());
        }
        report.cases.push(rec);
    }
    let update = fx.duration(Op::Update).expect("control update");
    let checkout = fx.duration(Op::Checkout).expect("control checkout");
    for op in Op::ALL {
        let d = fx.duration(op).expect("control case");
        eprintln!("{}: uninterrupted {} ms", op.name(), d.as_millis());
        for _ in 0..iters {
            let delay = d.mul_f64(rng.unit() * 1.1);
            let pre = match op {
                Op::Cleanup => Some(update.mul_f64(rng.unit())),
                Op::Continue => Some(checkout.mul_f64(rng.unit())),
                _ => None,
            };
            let rec = fx.case(op, &Kill::After(delay), pre);
            if let Some(f) = &rec.failure {
                eprintln!("FAIL case {} {} {}: {f}", rec.id, op.name(), rec.kill);
            }
            report.cases.push(rec);
        }
    }
    let path = report
        .write("sigkill", "Crash suite: random SIGKILL")
        .expect("report");
    eprintln!(
        "{}\nreport: {}",
        report.markdown("Crash suite: random SIGKILL"),
        path.display()
    );
    assert!(
        report.failures().is_empty(),
        "{} of {} crash cases failed (seed {seed}); see {}",
        report.failures().len(),
        report.cases.len(),
        path.display()
    );
}
