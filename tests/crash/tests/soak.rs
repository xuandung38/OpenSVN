//! Engine soak: one process runs update, status, commit and log against the local Apache over and
//! over, while the svn CLI commits as another user, and its resident memory must not grow.
//!
//! Every cycle: svn commits a change of `SOAK_FILES / 50` files from its own working copy
//! (another user), the engine updates its working copy (merging into its local edits), edits
//! `SOAK_FILES / 50` other files and commits them, runs status, and reads the newest 100 log
//! entries with changed paths. The RSS of this process is sampled after every cycle. The
//! baseline is the RSS once the warm-up (the first 10% of the run, at least one cycle) is over;
//! the RSS at the end must be less than 10% above it.
//!
//! `cargo test --release -p opensvn-crash --test soak -- --ignored --nocapture soak_2h`
//! (`soak_short` runs 2 minutes). Knobs: `SOAK_SECS` (overrides the duration of either test),
//! `SOAK_FILES` (default 2000), `CRASH_REPORT_DIR` (the report `soak.md` goes there).
#![cfg(target_os = "linux")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use opensvn_crash::engine::Engine;
use opensvn_crash::harness::env_count;
use opensvn_crash::server::Apache;
use opensvn_crash::svn::Svn;

/// Resident set size of this process in KiB.
fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("VmRSS:"))
                .and_then(|v| v.split_whitespace().next()?.parse().ok())
        })
        .unwrap_or(0)
}

fn file(i: usize) -> String {
    format!("d{:02}/f{i:05}.txt", i % 20)
}

fn write(path: &Path, text: &str) {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).expect("mkdir");
    }
    std::fs::write(path, text).expect("write");
}

/// Rewrites file `i` with a new last line of fixed width, so the working copy and every text
/// stay the same size from cycle to cycle (only memory the engine keeps may grow).
fn touch(path: &Path, i: usize, who: &str, cycle: usize) {
    std::fs::write(path, format!("file {i}\n{who} {cycle:010}\n")).expect("write");
}

struct Sample {
    at: Duration,
    cycle: usize,
    rss: u64,
}

fn soak(default_secs: u64, name: &str) {
    let secs = env_count("SOAK_SECS", default_secs as usize) as u64;
    let files = env_count("SOAK_FILES", 2000).max(100);
    let Some(svn) = Svn::find().expect("svn") else {
        return;
    };
    if !Apache::available() {
        assert!(
            !std::env::var("OPENSVN_REQUIRE_SVN").is_ok_and(|v| v == "1" || v == "true"),
            "apache2 + mod_dav_svn not installed"
        );
        eprintln!("skipping: apache2 + mod_dav_svn not installed");
        return;
    }
    let apache = Apache::start().expect("apache");
    let scratch = tempfile::Builder::new()
        .prefix("opensvn-soak-")
        .tempdir()
        .expect("tempdir");
    let base = scratch.path();
    svn.create_repo(&apache.repos_dir().join("soak"))
        .expect("create repo");
    apache.hand_over("soak").expect("hand over");
    let url = apache.url("soak");
    let import = base.join("import");
    for i in 0..files {
        write(&import.join(file(i)), &format!("file {i}\n"));
    }
    svn.run(&["import", "-q", "-m", "r1", &import.to_string_lossy(), &url])
        .expect("import");
    let theirs = base.join("theirs");
    svn.run(&["checkout", "-q", &url, &theirs.to_string_lossy()])
        .expect("svn checkout");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let engine = Engine::new(&base.join("state")).expect("engine");
    let ours: PathBuf = base.join("ours");
    rt.block_on(engine.checkout(&url, &ours, None))
        .expect("engine checkout");

    let per_cycle = files / 50;
    let total = Duration::from_secs(secs);
    let warmup = (total / 10).max(Duration::from_secs(1));
    let start = Instant::now();
    let mut samples: Vec<Sample> = Vec::new();
    let mut baseline: Option<(usize, u64)> = None;
    let mut cycle = 0;
    let mut last_report = Instant::now();
    while cycle == 0 || start.elapsed() < total {
        cycle += 1;
        // Another user's commit: the first half of the files, round robin.
        for k in 0..per_cycle {
            let i = (cycle * per_cycle + k) % (files / 2);
            touch(&theirs.join(file(i)), i, "theirs", cycle);
        }
        svn.run_in(&theirs, &["commit", "-q", "-m", &format!("theirs {cycle}")])
            .expect("svn commit");
        rt.block_on(async {
            engine.update(&url, &ours, None).await.expect("update");
            // Our edits: the second half.
            for k in 0..per_cycle {
                let i = files / 2 + (cycle * per_cycle + k) % (files / 2);
                touch(&ours.join(file(i)), i, "ours", cycle);
            }
            let st = engine.status(&ours).await.expect("status");
            assert_eq!(st.len(), per_cycle, "status before commit: {st:?}");
            engine
                .commit(&url, &ours, &format!("ours {cycle}"))
                .await
                .expect("commit")
                .expect("a new revision");
            let st = engine.status(&ours).await.expect("status");
            assert!(st.is_empty(), "status after commit: {st:?}");
            let (entries, changed) = engine.log(&url, "", Some(100), true).await.expect("log");
            assert_eq!(entries, 100.min(2 * cycle + 1));
            assert!(
                changed >= entries,
                "{changed} changed paths in {entries} entries"
            );
        });
        svn.run_in(&theirs, &["update", "-q"]).expect("svn update");
        let rss = rss_kib();
        samples.push(Sample {
            at: start.elapsed(),
            cycle,
            rss,
        });
        if baseline.is_none() && start.elapsed() >= warmup {
            baseline = Some((cycle, rss));
        }
        if last_report.elapsed() >= Duration::from_secs(60) {
            last_report = Instant::now();
            eprintln!(
                "{:>6.0} s: cycle {cycle}, RSS {} KiB",
                start.elapsed().as_secs_f64(),
                rss
            );
        }
    }
    let elapsed = start.elapsed();
    let (base_cycle, base_rss) = baseline.unwrap_or((1, samples[0].rss));
    let end = samples.last().expect("a sample");
    let max = samples.iter().map(|s| s.rss).max().unwrap_or(0);
    let growth = (end.rss as f64 - base_rss as f64) / base_rss as f64;
    // svn reads the engine's working copy and the history it wrote.
    let st = svn.status(&ours).expect("svn status of our working copy");
    assert!(st.is_empty(), "svn status after the soak: {st:?}");

    let mut md = format!(
        "# Engine soak ({name})\n\n- duration: {:.0} s, {} cycles of update + status + commit + status + log -l 100 -v (and one svn commit by another user)\n- files: {files}, {per_cycle} changed per cycle by each side\n- machine: {} CPUs, build: {}\n- RSS at the end of warm-up (cycle {base_cycle}): {base_rss} KiB\n- RSS at the end (cycle {}): {} KiB, growth {:+.1}% (limit +10%)\n- max RSS: {max} KiB\n\n| time | cycle | RSS (KiB) |\n|---:|---:|---:|\n",
        elapsed.as_secs_f64(),
        end.cycle,
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        end.cycle,
        end.rss,
        growth * 100.0
    );
    // About 20 rows: the samples spread over the run, plus the last one.
    let step = (samples.len() / 20).max(1);
    for s in samples.iter().step_by(step).chain(std::iter::once(end)) {
        let _ = writeln!(
            md,
            "| {:.0} s | {} | {} |",
            s.at.as_secs_f64(),
            s.cycle,
            s.rss
        );
    }
    let dir = std::env::var_os("CRASH_REPORT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/crash-report")
        });
    std::fs::create_dir_all(&dir).expect("report dir");
    let path = dir.join(format!("{name}.md"));
    std::fs::write(&path, &md).expect("report");
    eprintln!("{md}\nreport: {}", path.display());
    assert!(
        growth < 0.10,
        "RSS grew {:.1}% (from {base_rss} KiB after warm-up to {} KiB); see {}",
        growth * 100.0,
        end.rss,
        path.display()
    );
}

#[test]
#[ignore = "two hours; needs apache2 + mod_dav_svn and svn"]
fn soak_2h() {
    soak(2 * 3600, "soak-2h");
}

#[test]
#[ignore = "needs apache2 + mod_dav_svn and svn"]
fn soak_short() {
    soak(120, "soak-short");
}
