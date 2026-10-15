//! The engine and the svn 1.15 CLI on one working copy at the same time.
//!
//! Each scenario pairs an engine operation with an svn operation (update vs status, commit vs
//! cleanup, update vs update, commit vs commit, ...). Both run in loops at the same time on one
//! working copy of a repository served by the local Apache, the engine in this process and svn
//! as child processes. Then:
//!
//! - neither side ever fails for any reason but the other side's lock: the engine with a typed
//!   [`ErrorCode::WcBusy`] (an engine error that mentions a lock or a busy database under any
//!   other code fails the scenario), svn with `E155004` (working copy locked), `E200033`
//!   (database busy) or `E155037` (svn checks for queued work before it looks at the lock rows,
//!   `wclock_obtain` in `wc_db.c`, so a writer that holds the lock with work queued reads as
//!   "previous operation has not finished", against another svn just the same); an out-of-date
//!   commit is accepted where both sides commit. `svn cleanup` breaks every lock (below): an
//!   engine operation it breaks may fail on its vanished temporary files, and svn cleanup may
//!   fail on the engine's; those are counted apart;
//! - the working copy is never corrupt: `wc.db` passes the integrity checks, no lock row or work
//!   item is left, `svn status` and the engine's status agree;
//! - nothing is lost: every line either side appended reaches the repository, and the working
//!   copy then equals a fresh checkout.
//!
//! Two deterministic cases show that locks are honoured both ways: while the engine holds the
//! working copy lock, every writing svn command fails with `E155004` and `svn status` works
//! (except `svn cleanup`, which always breaks locks: `cleanup-cmd.c` passes `break_locks = TRUE`,
//! against another svn just the same);
//! while svn's lock row is in `wc.db`, every writing engine operation fails with `WcBusy`,
//! changes nothing, and the engine's status works.
//!
//! `cargo test -p opensvn-crash --test concurrency -- --ignored --nocapture`
//! Knobs: `CONC_ROUNDS` (operations per side and scenario, default 12), `CONC_FILES` (default
//! 300), `CRASH_REPORT_DIR` (report `concurrency.md`).
#![cfg(target_os = "linux")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier, Mutex};

use opensvn_crash::engine::Engine;
use opensvn_crash::harness::env_count;
use opensvn_crash::server::Apache;
use opensvn_crash::svn::Svn;
use opensvn_crash::tree::snapshot;
use opensvn_crash::verify;
use svn_types::{CancelToken, ErrorCode, Revnum, SvnError};

/// What one side runs in its loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    /// Update; with `toggle` alternately to r1 and HEAD, else always to HEAD.
    Update,
    /// Local status.
    Status,
    /// Append a line to one of this side's files, then commit the working copy.
    Commit,
    /// Cleanup.
    Cleanup,
}

impl Op {
    fn name(self) -> &'static str {
        match self {
            Op::Update => "update",
            Op::Status => "status",
            Op::Commit => "commit",
            Op::Cleanup => "cleanup",
        }
    }
}

/// How the runs of one side ended.
#[derive(Debug, Default, Clone)]
struct Tally {
    ok: usize,
    busy: usize,
    out_of_date: usize,
    /// Failed because `svn cleanup` broke the lock and removed the temporary files in use.
    broken: usize,
    failures: Vec<String>,
    /// `(file, line)` appended before a commit (whether or not that commit went through).
    edits: Vec<(String, String)>,
}

fn file(i: usize) -> String {
    format!("d{}/f{i:04}.txt", i % 10)
}

fn append(wc: &Path, rel: &str, line: &str) {
    let p = wc.join(rel);
    let mut t = std::fs::read_to_string(&p).expect("read");
    t.push_str(line);
    t.push('\n');
    std::fs::write(&p, t).expect("write");
}

/// Sorts an engine result into the tally. Busy must be typed.
fn engine_outcome(
    t: &mut Tally,
    what: &str,
    r: std::result::Result<(), SvnError>,
    commits: bool,
    against_cleanup: bool,
) {
    match r {
        Ok(()) => t.ok += 1,
        Err(e) if e.code == ErrorCode::WcBusy => t.busy += 1,
        Err(e) if commits && e.code == ErrorCode::OutOfDate => t.out_of_date += 1,
        Err(e)
            if against_cleanup
                && e.code == ErrorCode::PathNotFound
                && (e.to_string().contains(".svn/tmp")
                    || e.to_string().contains(".svn/pristine")) =>
        {
            t.broken += 1;
        }
        Err(e) => {
            let text = e.to_string().to_lowercase();
            let hint = if text.contains("lock") || text.contains("busy") {
                " (a busy/lock error without the WcBusy code)"
            } else {
                ""
            };
            t.failures
                .push(format!("engine {what}: {:?}: {e}{hint}", e.code));
        }
    }
}

/// Sorts an svn result into the tally.
fn svn_outcome(t: &mut Tally, what: &str, out: &std::process::Output, commits: bool) {
    let err = String::from_utf8_lossy(&out.stderr);
    if out.status.success() {
        t.ok += 1;
    } else if err.contains("E155004") || err.contains("E200033") || err.contains("E155037") {
        t.busy += 1;
    } else if what == "cleanup"
        && (err.contains("E000002") || err.contains("E000039"))
        && err.contains(".svn/tmp")
    {
        t.broken += 1;
    } else if commits
        && ["E155011", "E160028", "E170004", "E160024"]
            .iter()
            .any(|c| err.contains(c))
    {
        t.out_of_date += 1;
    } else {
        t.failures.push(format!("svn {what}: {}", err.trim()));
    }
}

struct Fixture {
    apache: Apache,
    svn: Svn,
    scratch: tempfile::TempDir,
    files: usize,
}

impl Fixture {
    fn new() -> Option<Self> {
        let svn = Svn::find().expect("svn")?;
        if !Apache::available() {
            assert!(
                !std::env::var("OPENSVN_REQUIRE_SVN").is_ok_and(|v| v == "1" || v == "true"),
                "apache2 + mod_dav_svn not installed"
            );
            eprintln!("skipping: apache2 + mod_dav_svn not installed");
            return None;
        }
        let (x, y, _) = svn.version();
        if (x, y) < (1, 15) {
            assert!(
                !std::env::var("OPENSVN_REQUIRE_SVN").is_ok_and(|v| v == "1" || v == "true"),
                "svn {x}.{y} found, the concurrency suite needs svn 1.15"
            );
            eprintln!("skipping: svn {x}.{y} found, the concurrency suite needs svn 1.15");
            return None;
        }
        let apache = Apache::start().expect("apache");
        let scratch = tempfile::Builder::new()
            .prefix("opensvn-conc-")
            .tempdir()
            .expect("tempdir");
        let me = Self {
            apache,
            svn,
            scratch,
            files: env_count("CONC_FILES", 300).max(40),
        };
        // Template repository: r1 = the files, r2 = another user's change of every tenth.
        me.svn
            .create_repo(&me.apache.repos_dir().join("tpl"))
            .expect("create");
        me.apache.hand_over("tpl").expect("hand over");
        let url = me.apache.url("tpl");
        let import = me.path("import");
        for i in 0..me.files {
            let p = import.join(file(i));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, format!("file {i}\n")).unwrap();
        }
        me.svn
            .run(&["import", "-q", "-m", "r1", &import.to_string_lossy(), &url])
            .expect("import");
        let other = me.path("other");
        me.svn
            .run(&["checkout", "-q", &url, &other.to_string_lossy()])
            .expect("checkout");
        for i in (0..me.files).step_by(10) {
            append(&other, &file(i), "changed in r2");
        }
        me.svn
            .run_in(&other, &["commit", "-q", "-m", "r2"])
            .expect("r2");
        Some(me)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.scratch.path().join(name)
    }

    /// A copy of the template repository and an svn checkout of it at HEAD.
    fn case(&self, name: &str) -> (String, PathBuf) {
        self.apache.copy_repo("tpl", name).expect("copy repo");
        let url = self.apache.url(name);
        let wc = self.path(&format!("wc-{name}"));
        self.svn
            .run(&["checkout", "-q", &url, &wc.to_string_lossy()])
            .expect("checkout");
        (url, wc)
    }
}

struct Scenario {
    engine: Op,
    svn: Op,
    /// Updates alternate between r1 and HEAD (only where nobody commits).
    toggle: bool,
}

fn run_engine(
    s: &Scenario,
    rounds: usize,
    url: &str,
    wc: &Path,
    state: &Path,
    files: usize,
    start: &Barrier,
) -> Tally {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let engine = Engine::new(state).unwrap();
    let mut t = Tally::default();
    let (op, toggle) = (s.engine, s.toggle);
    let commits = op == Op::Commit;
    start.wait();
    for k in 0..rounds {
        let r = match op {
            Op::Update => {
                let rev = (toggle && k % 2 == 0).then_some(Revnum(1));
                rt.block_on(engine.update(url, wc, rev)).map(|_| ())
            }
            Op::Status => rt.block_on(engine.status(wc)).map(|_| ()),
            Op::Commit => {
                // Even files below the first half are the engine's.
                let rel = file((2 * k) % (files / 2));
                let line = format!("engine round {k}");
                append(wc, &rel, &line);
                t.edits.push((rel, line));
                rt.block_on(engine.commit(url, wc, &format!("engine {k}")))
                    .map(|_| ())
            }
            Op::Cleanup => engine.cleanup(wc).map(|_| ()),
        };
        engine_outcome(&mut t, op.name(), r, commits, s.svn == Op::Cleanup);
    }
    t
}

fn run_svn(
    svn: &Svn,
    op: Op,
    toggle: bool,
    rounds: usize,
    wc: &Path,
    files: usize,
    start: &Barrier,
) -> Tally {
    let mut t = Tally::default();
    let commits = op == Op::Commit;
    start.wait();
    for k in 0..rounds {
        let mut c = svn.cmd();
        c.current_dir(wc);
        match op {
            Op::Update if toggle && k % 2 == 0 => c.args(["update", "-q", "-r", "1"]),
            Op::Update => c.args(["update", "-q"]),
            Op::Status => c.args(["status"]),
            Op::Commit => {
                // Odd files below the first half are svn's.
                let rel = file((2 * k + 1) % (files / 2));
                let line = format!("svn round {k}");
                append(wc, &rel, &line);
                t.edits.push((rel, line));
                c.args(["commit", "-q", "-m", &format!("svn {k}")])
            }
            Op::Cleanup => c.args(["cleanup"]),
        };
        let out = c.output().expect("svn");
        svn_outcome(&mut t, op.name(), &out, commits);
    }
    t
}

/// The working copy after both sides stopped: intact, quiescent, both statuses agree; then once
/// the last local edits are committed, every edit is in the repository and the working copy
/// equals a fresh checkout.
fn check_after(
    fx: &Fixture,
    name: &str,
    url: &str,
    wc: &Path,
    state: &Path,
    edits: &[(String, String)],
) -> Vec<String> {
    let mut problems = Vec::new();
    if let Err(e) = verify::db_integrity(wc) {
        problems.push(format!("wc.db: {e}"));
        return problems;
    }
    if let Err(e) = verify::db_quiescent(wc) {
        problems.push(format!("left behind: {e}"));
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let engine = Engine::new(state).unwrap();
    let theirs = match fx.svn.status(wc) {
        Ok(s) => s,
        Err(e) => {
            problems.push(format!("svn cannot read the working copy: {e}"));
            return problems;
        }
    };
    match rt.block_on(engine.status(wc)) {
        Ok(list) => {
            let ours = verify::engine_status_map(&list);
            let d = verify::status_diff("engine", &ours, "svn", &theirs);
            if !d.is_empty() {
                problems.push(format!("status differs: {}", d.join("; ")));
            }
        }
        Err(e) => problems.push(format!("engine status: {e}")),
    }
    // Bring everything to HEAD: commit what is left (engine), update both ways.
    if let Err(e) = rt.block_on(engine.commit(url, wc, &format!("{name}: rest"))) {
        problems.push(format!("final engine commit: {e}"));
    }
    if let Err(e) = rt.block_on(engine.update(url, wc, None)) {
        problems.push(format!("final engine update: {e}"));
    }
    if let Err(e) = fx.svn.run_in(wc, &["update", "-q"]) {
        problems.push(format!("final svn update: {e}"));
    }
    match fx.svn.status(wc) {
        Ok(s) if s.is_empty() => {}
        Ok(s) => problems.push(format!("not clean at the end: {s:?}")),
        Err(e) => problems.push(format!("final svn status: {e}")),
    }
    for (rel, line) in edits {
        match fx.svn.cat(&format!("{url}/{rel}")) {
            Ok(bytes) if String::from_utf8_lossy(&bytes).lines().any(|l| l == line) => {}
            Ok(_) => problems.push(format!("lost edit: {rel}: {line:?}")),
            Err(e) => problems.push(format!("svn cat {rel}: {e}")),
        }
    }
    let fresh = fx.path(&format!("fresh-{name}"));
    fx.svn
        .run(&["checkout", "-q", url, &fresh.to_string_lossy()])
        .expect("fresh checkout");
    if snapshot(wc).expect("snapshot") != snapshot(&fresh).expect("snapshot") {
        problems.push("the working copy differs from a fresh checkout of HEAD".into());
    }
    problems
}

#[test]
#[ignore = "needs apache2 + mod_dav_svn and svn 1.15"]
fn engine_and_svn_on_one_working_copy() {
    let Some(fx) = Fixture::new() else { return };
    let rounds = env_count("CONC_ROUNDS", 12);
    let scenarios = [
        Scenario {
            engine: Op::Update,
            svn: Op::Status,
            toggle: true,
        },
        Scenario {
            engine: Op::Status,
            svn: Op::Update,
            toggle: true,
        },
        Scenario {
            engine: Op::Update,
            svn: Op::Update,
            toggle: true,
        },
        Scenario {
            engine: Op::Commit,
            svn: Op::Cleanup,
            toggle: false,
        },
        Scenario {
            engine: Op::Cleanup,
            svn: Op::Commit,
            toggle: false,
        },
        Scenario {
            engine: Op::Commit,
            svn: Op::Commit,
            toggle: false,
        },
        Scenario {
            engine: Op::Update,
            svn: Op::Commit,
            toggle: false,
        },
        Scenario {
            engine: Op::Commit,
            svn: Op::Update,
            toggle: false,
        },
        Scenario {
            engine: Op::Commit,
            svn: Op::Status,
            toggle: false,
        },
        Scenario {
            engine: Op::Cleanup,
            svn: Op::Update,
            toggle: true,
        },
        Scenario {
            engine: Op::Update,
            svn: Op::Cleanup,
            toggle: true,
        },
    ];
    let mut md = format!(
        "# Engine and svn {} on one working copy\n\n{} files, {rounds} operations per side and scenario. Outcomes: ok / busy (typed) / out of date / broken by `svn cleanup`.\n\n| engine | svn | engine | svn | problems |\n|---|---|---|---|---:|\n",
        {
            let (x, y, z) = fx.svn.version();
            format!("{x}.{y}.{z}")
        },
        fx.files
    );
    let mut all_problems = Vec::new();
    for (n, s) in scenarios.iter().enumerate() {
        let name = format!("c{n}-{}-{}", s.engine.name(), s.svn.name());
        let (url, wc) = fx.case(&name);
        let state = fx.path(&format!("state-{name}"));
        let start = Arc::new(Barrier::new(2));
        let svn_tally = Arc::new(Mutex::new(Tally::default()));
        let theirs = {
            let (svn, wc, start, tally) = (
                fx.svn.clone(),
                wc.clone(),
                Arc::clone(&start),
                Arc::clone(&svn_tally),
            );
            let (op, toggle, files) = (s.svn, s.toggle, fx.files);
            std::thread::spawn(move || {
                let t = run_svn(&svn, op, toggle, rounds, &wc, files, &start);
                *tally.lock().unwrap() = t;
            })
        };
        let ours = run_engine(s, rounds, &url, &wc, &state, fx.files, &start);
        theirs.join().expect("svn thread");
        let theirs = svn_tally.lock().unwrap().clone();
        let mut problems: Vec<String> = ours
            .failures
            .iter()
            .chain(theirs.failures.iter())
            .cloned()
            .collect();
        let edits: Vec<(String, String)> = ours
            .edits
            .iter()
            .chain(theirs.edits.iter())
            .cloned()
            .collect();
        problems.extend(check_after(&fx, &name, &url, &wc, &state, &edits));
        if ours.ok == 0 && theirs.ok == 0 {
            problems.push("neither side ever succeeded".into());
        }
        eprintln!(
            "{name}: engine {}/{}/{} svn {}/{}/{} problems {}",
            ours.ok,
            ours.busy,
            ours.out_of_date,
            theirs.ok,
            theirs.busy,
            theirs.out_of_date,
            problems.len()
        );
        let _ = writeln!(
            md,
            "| {} | {} | {} / {} / {} / {} | {} / {} / {} / {} | {} |",
            s.engine.name(),
            s.svn.name(),
            ours.ok,
            ours.busy,
            ours.out_of_date,
            ours.broken,
            theirs.ok,
            theirs.busy,
            theirs.out_of_date,
            theirs.broken,
            problems.len()
        );
        all_problems.extend(problems.into_iter().map(|p| format!("{name}: {p}")));
    }
    if !all_problems.is_empty() {
        md.push_str("\n## Problems\n\n");
        for p in all_problems.iter().take(50) {
            let _ = writeln!(md, "- {p}");
        }
    }
    let dir = std::env::var_os("CRASH_REPORT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/crash-report")
        });
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("concurrency.md");
    std::fs::write(&path, &md).unwrap();
    eprintln!("{md}\nreport: {}", path.display());
    assert!(
        all_problems.is_empty(),
        "{} problems; see {}",
        all_problems.len(),
        path.display()
    );
}

/// Engine and svn status as `path -> columns`, plus the files on disk.
fn state_of(
    fx: &Fixture,
    wc: &Path,
) -> (
    BTreeMap<String, String>,
    BTreeMap<String, opensvn_crash::tree::Entry>,
) {
    (
        fx.svn.status(wc).expect("svn status"),
        snapshot(wc).expect("snapshot"),
    )
}

#[test]
#[ignore = "needs apache2 + mod_dav_svn and svn 1.15"]
fn locks_are_honoured_both_ways() {
    let Some(fx) = Fixture::new() else { return };
    let (url, wc) = fx.case("locks");
    let state = fx.path("state-locks");
    let engine = Engine::new(&state).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // File 1 is not changed in r2: updating it to r1 and back merges nothing.
    append(&wc, &file(1), "local edit");

    // 1. The engine holds the working copy lock: svn may read but not write.
    {
        let db = engine.open(&wc).unwrap();
        let before = state_of(&fx, &wc);
        svn_client::local::lock::with_wc_lock(&db, &CancelToken::new(), || {
            for args in [
                vec!["update", "-q", "-r", "1"],
                vec!["commit", "-q", "-m", "while locked"],
                vec!["revert", "-q", "-R", "."],
                vec!["propset", "-q", "p", "v", "d0"],
            ] {
                let out = fx.svn.cmd().current_dir(&wc).args(&args).output().unwrap();
                let err = String::from_utf8_lossy(&out.stderr);
                assert!(
                    !out.status.success() && err.contains("E155004"),
                    "svn {args:?} while the engine holds the lock: {} {err}",
                    out.status
                );
            }
            let st = fx
                .svn
                .status(&wc)
                .expect("svn status while the engine holds the lock");
            assert_eq!(st.get(&file(1)).map(String::as_str), Some("M  "), "{st:?}");
            Ok(())
        })
        .expect("engine lock");
        assert_eq!(
            state_of(&fx, &wc),
            before,
            "svn changed a locked working copy"
        );
    }
    verify::db_quiescent(&wc).expect("engine lock released");
    fx.svn
        .run_in(&wc, &["update", "-q", "-r", "1"])
        .expect("svn update once the engine released the lock");

    // 2. svn's lock row (what svn writes while it runs; svn takes and releases it inside one
    // command, so the row is put in directly): the engine may read but not write.
    {
        let c = rusqlite::Connection::open(wc.join(".svn/wc.db")).unwrap();
        c.execute(
            "INSERT INTO wc_lock (wc_id, local_dir_relpath, locked_levels) VALUES (1, '', -1)",
            [],
        )
        .unwrap();
    }
    let before = state_of(&fx, &wc);
    let busy = |what: &str, r: Result<(), SvnError>| match r {
        Err(e) if e.code == ErrorCode::WcBusy => {}
        other => panic!("engine {what} while svn holds the lock: {other:?}"),
    };
    busy(
        "update",
        rt.block_on(engine.update(&url, &wc, None)).map(|_| ()),
    );
    busy(
        "commit",
        rt.block_on(engine.commit(&url, &wc, "while locked"))
            .map(|_| ()),
    );
    busy("cleanup", engine.cleanup(&wc).map(|_| ()));
    busy("revert", engine.revert(&wc).map(|_| ()));
    let list = rt
        .block_on(engine.status(&wc))
        .expect("engine status while svn holds the lock");
    let ours = verify::engine_status_map(&list);
    assert_eq!(
        ours.get(&file(1)).map(String::as_str),
        Some("M  "),
        "{ours:?}"
    );
    assert_eq!(
        state_of(&fx, &wc),
        before,
        "the engine changed a locked working copy"
    );
    // Recovery leaves a lock it cannot prove is its own to the user.
    let rec = rt.block_on(engine.recover(&url, &wc)).expect("recover");
    assert_eq!(rec.locks_removed, 0, "{rec:?}");
    assert!(rec.locks_need_user > 0, "{rec:?}");
    fx.svn.run_in(&wc, &["cleanup"]).expect("svn cleanup");
    rt.block_on(engine.update(&url, &wc, None))
        .expect("engine update once svn's lock is gone");
    let st = fx.svn.status(&wc).unwrap();
    assert_eq!(st.get(&file(1)).map(String::as_str), Some("M  "), "{st:?}");
}
