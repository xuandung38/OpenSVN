//! Crash cases: a fixture repository on the local Apache, template working copies, a worker
//! process killed at a random moment or at a named failpoint, then recovery and verification.
//!
//! Fixture (built with the svn CLI): `r1` holds `files` text files of ten lines in ten
//! directories plus two binaries; `r2` (another user's commit) changes the last line of every
//! third file, deletes some files, adds some, and rewrites a binary. It also branches `d0` to
//! `d0-branch` (a copy of `d0@1`) whose files get one more line 5 change and the same last-line
//! change as r2's `d0`, plus one file of its own. Templates:
//!
//! * `tpl-r1`: checked out at r1 with local edits on the first line of every fourth file (some
//!   of them also changed on the server: clean merges), two files whose last line is edited
//!   locally and on the server (text conflicts), and an unversioned file. Used by `update` and
//!   `cleanup` ("mid-update with local edits"), and by `switch` (`d0` to `d0-branch`, with local
//!   edits merged and one text conflict).
//! * `tpl-r2`: checked out at HEAD with local edits, a locally rewritten binary, an added
//!   directory with files and deleted files. Used by `commit` and `revert`.
//!
//! Every case runs on its own copy of the repository and of the template, so cases are
//! independent and can be replayed from the printed seed.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::engine::Engine;
use crate::rng::Rng;
use crate::server::Apache;
use crate::svn::Svn;
use crate::tree::{Entry, copy_dir, snapshot};
use crate::worker::{FP_ENV, FP_HIT_ENV};
use crate::{Result, err, verify};

/// Every failpoint of `svn-wc` and `svn-client` (feature `failpoints`).
pub const FAILPOINTS: &[&str] = &[
    "wc.create.before_schema",
    "wc.create.before_rename",
    "pristine.install.before_rename",
    "wc.txn.before_commit",
    "wc.txn.after_commit",
    "wc.wq.after_delete",
    "wc.wq.after_fileinfo",
    "update.after_delete_txn",
    "update.after_merge3_before_txn",
    "update.after_txn_before_queue",
    "update.after_merge3_before_markers",
    "update.sync_barrier",
    "update.barrier_wait",
    "commit.after_txn_created",
    "commit.after_merge_sent_marker",
    "commit.journal_merged_write",
    "commit.after_merge_before_finalize",
    "commit.after_finalize_before_queue",
    "commit.finalize",
];

/// The directory of `tpl-r1` the `switch` cases switch.
pub const SWITCH_DIR: &str = "d0";
/// Where `switch` switches [`SWITCH_DIR`] to: a branch of it made in r2.
pub const SWITCH_BRANCH: &str = "d0-branch";
/// A file that only the branch has (`<SWITCH_BRANCH>/<name>`).
pub const BRANCH_ONLY: &str = "only-on-branch.txt";

/// The operation a worker runs and the suite kills.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Op {
    /// Checkout of HEAD into an empty directory.
    Checkout,
    /// Update r1 -> r2 of a working copy with local edits (clean merges and conflicts).
    Update,
    /// Switch of `d0` of the same working copy to `d0-branch` at r2.
    Switch,
    /// Commit of edits, adds, deletes and a binary change.
    Commit,
    /// Recursive revert of the same changes.
    Revert,
    /// Cleanup of a working copy whose update was killed before.
    Cleanup,
    /// "Continue checkout" (the app's `wc_continue`) of a checkout that was killed before:
    /// recovery of the dead process's lock, then an update to the revision the incomplete
    /// folders record.
    Continue,
}

impl Op {
    /// All operations.
    pub const ALL: [Op; 7] = [
        Op::Checkout,
        Op::Update,
        Op::Switch,
        Op::Commit,
        Op::Revert,
        Op::Cleanup,
        Op::Continue,
    ];

    /// Worker op name.
    pub fn name(self) -> &'static str {
        match self {
            Op::Checkout => "checkout",
            Op::Update => "update",
            Op::Switch => "switch",
            Op::Commit => "commit",
            Op::Revert => "revert",
            Op::Cleanup => "cleanup",
            Op::Continue => "continue",
        }
    }
}

/// How the worker is stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kill {
    /// Runs to completion (control case, calibration).
    Never,
    /// `SIGKILL` from the parent after this delay.
    After(Duration),
    /// The worker kills itself at hit `hit` of failpoint `name` (`hit == 0`: count only, the
    /// name may be a comma separated list).
    Failpoint {
        /// Failpoint name(s).
        name: String,
        /// 1-based hit, 0 = count.
        hit: u64,
    },
}

impl std::fmt::Display for Kill {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Kill::Never => f.write_str("none"),
            Kill::After(d) => write!(f, "sigkill@{}ms", d.as_millis()),
            Kill::Failpoint { name, hit } => write!(f, "{name}#{hit}"),
        }
    }
}

/// How a worker ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    /// Killed by `SIGKILL`.
    Killed,
    /// Exited with `RESULT ok ...`; the details.
    Ok(String),
    /// Exited with an error; the message.
    Failed(String),
}

/// A finished worker process.
#[derive(Debug, Clone)]
pub struct WorkerRun {
    /// How it ended.
    pub ended: Ended,
    /// Wall time.
    pub elapsed: Duration,
    /// Whole stdout.
    pub stdout: String,
    /// Whole stderr.
    pub stderr: String,
}

impl WorkerRun {
    /// Failpoint hit counts printed in count mode (`FP_HITS <n>` with a single name).
    pub fn fp_hits(&self) -> Option<u64> {
        self.stdout
            .lines()
            .find_map(|l| l.strip_prefix("FP_HITS ")?.trim().parse().ok())
    }

    fn require_ok(&self, what: &str) -> Result<&str> {
        match &self.ended {
            Ended::Ok(d) => Ok(d),
            Ended::Killed => Err(err(format!("{what}: worker was killed unexpectedly"))),
            Ended::Failed(m) => Err(err(format!("{what} failed: {m}\n{}", self.stderr))),
        }
    }
}

/// Outcome of one crash case.
#[derive(Debug, Clone)]
pub struct CaseRecord {
    /// Case number (also the repository name suffix).
    pub id: u64,
    /// Operation.
    pub op: Op,
    /// Kill mode.
    pub kill: Kill,
    /// How the worker under test ended.
    pub ended: Ended,
    /// What recovery reported (`recover` details), when it ran.
    pub recovery: Option<String>,
    /// First failed check, `None` when every check passed.
    pub failure: Option<String>,
    /// Hits of the failpoint in a count-only run.
    pub fp_hits: Option<u64>,
    /// Non-fatal observations (garbage left in `.svn/tmp`, ...).
    pub warnings: Vec<String>,
    /// Wall time of the whole case.
    pub elapsed: Duration,
    /// Recovery left a lock row of unknown owner and needed a manual `svn cleanup`.
    pub manual_cleanup: bool,
}

/// Whether `kill` may land between the commit of a WC_LOCK row and its ownership record, the
/// one window where recovery leaves the lock for the user (`svn-wc` `wc_lock.rs`: the row is
/// committed first so that a crash never makes a foreign lock look like ours): a random kill or
/// a kill right after a transaction commits, and the process died before it wrote the lock's
/// marker into `.svn/tmp` (the marker is written last).
pub fn lock_row_window(kill: &Kill, wc: &Path) -> bool {
    let after_commit = match kill {
        Kill::After(_) => true,
        Kill::Failpoint { name, .. } => name == "wc.txn.after_commit",
        Kill::Never => false,
    };
    let marker = std::fs::read_dir(wc.join(".svn/tmp")).is_ok_and(|rd| {
        rd.filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().starts_with("opensvn-lock-"))
    });
    after_commit && !marker
}

/// Text file contents: ten lines; line 1 and line 10 can be replaced.
fn text(path: &str, first: Option<&str>, last: Option<&str>) -> Vec<u8> {
    text5(path, first, None, last)
}

/// [`text`] with line 5 replaced as well.
fn text5(path: &str, first: Option<&str>, fifth: Option<&str>, last: Option<&str>) -> Vec<u8> {
    let mut s = String::new();
    for k in 1..=10 {
        let line = match k {
            1 => first.map(str::to_owned),
            5 => fifth.map(str::to_owned),
            10 => last.map(str::to_owned),
            _ => None,
        }
        .unwrap_or_else(|| format!("line {k} of {path}"));
        s.push_str(&line);
        s.push('\n');
    }
    s.into_bytes()
}

/// Line 1 of a locally edited file.
pub fn local_line(path: &str) -> String {
    format!("local edit of {path}")
}

/// Line 10 of a file changed in r2.
pub fn server_line(path: &str) -> String {
    format!("server edit of {path}")
}

/// Line 5 of a file of `d0` on the branch.
pub fn branch_line(path: &str) -> String {
    format!("branch edit of {path}")
}

/// Line 10 of a file edited locally where r2 also changes it (conflict).
pub fn conflict_line(path: &str) -> String {
    format!("conflicting local edit of {path}")
}

/// What the fixture contains and what each template changes locally.
#[derive(Debug, Clone, Default)]
pub struct Model {
    /// Text files of r1.
    pub files: Vec<String>,
    /// Binary files of r1 (1 MiB each).
    pub binaries: Vec<String>,
    /// Files whose last line r2 changes.
    pub server_modified: BTreeSet<String>,
    /// Files r2 deletes.
    pub server_deleted: BTreeSet<String>,
    /// Files r2 adds.
    pub server_added: Vec<String>,
    /// tpl-r1: first line edited locally.
    pub local_edits: BTreeSet<String>,
    /// tpl-r1: last line edited locally, conflicting with r2.
    pub conflicts: BTreeSet<String>,
    /// Text files of r1 below [`SWITCH_DIR`] (the branch has each of them).
    pub switched: BTreeSet<String>,
    /// tpl-r2: first line edited locally.
    pub commit_edits: BTreeSet<String>,
    /// tpl-r2: files added locally (below `added/`).
    pub commit_adds: Vec<String>,
    /// tpl-r2: files deleted locally.
    pub commit_deletes: BTreeSet<String>,
    /// tpl-r2: binary rewritten locally, with its new contents.
    pub commit_binary: (String, Vec<u8>),
    /// Binary contents at r1 and r2.
    pub binary_r1: BTreeMap<String, Vec<u8>>,
    /// Binary contents at r2.
    pub binary_r2: BTreeMap<String, Vec<u8>>,
}

impl Model {
    fn build(n: usize, rng: &mut Rng) -> Self {
        let mut m = Self::default();
        for i in 0..n {
            m.files.push(format!("d{}/f{i:05}.txt", i % 10));
        }
        let below = format!("{SWITCH_DIR}/");
        m.switched = m
            .files
            .iter()
            .filter(|f| f.starts_with(&below))
            .cloned()
            .collect();
        for (i, f) in m.files.iter().enumerate() {
            let deleted = i % 17 == 5;
            if deleted {
                m.server_deleted.insert(f.clone());
            } else if i % 3 == 0 {
                m.server_modified.insert(f.clone());
            }
            if !deleted && i % 4 == 1 {
                m.local_edits.insert(f.clone());
            }
        }
        // Two conflicts: files changed on the server and not otherwise edited locally.
        let conflicts: Vec<String> = m
            .server_modified
            .iter()
            .filter(|f| !m.local_edits.contains(*f))
            .take(2)
            .cloned()
            .collect();
        m.conflicts.extend(conflicts);
        for j in 0..(n / 20).max(2) {
            m.server_added.push(format!("d{}/new{j:04}.txt", j % 10));
        }
        let r2_files: Vec<String> = m.r2_text_files();
        for (i, f) in r2_files.iter().enumerate() {
            if i % 23 == 7 {
                m.commit_deletes.insert(f.clone());
            } else if i % 5 == 2 {
                m.commit_edits.insert(f.clone());
            }
        }
        for k in 0..3 {
            m.commit_adds.push(format!("added/a{k}.txt"));
        }
        for b in 0..2 {
            let name = format!("bin/blob{b}.bin");
            let r1 = rng.bytes(1 << 20);
            let mut r2 = r1.clone();
            if b == 0 {
                // r2 rewrites the middle of blob0: a delta, not a new file.
                let mid = r2.len() / 2;
                r2[mid..mid + 4096].copy_from_slice(&rng.bytes(4096));
            }
            m.binary_r1.insert(name.clone(), r1);
            m.binary_r2.insert(name.clone(), r2);
            m.binaries.push(name);
        }
        let b1 = m.binaries[1].clone();
        let mut local = m.binary_r2[&b1].clone();
        local[..4096].copy_from_slice(&rng.bytes(4096));
        m.commit_binary = (b1, local);
        m
    }

    /// Text files of r2.
    pub fn r2_text_files(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .files
            .iter()
            .filter(|f| !self.server_deleted.contains(*f))
            .cloned()
            .chain(self.server_added.iter().cloned())
            .collect();
        v.sort();
        v
    }

    /// Contents of text file `p` at r1 (`rev == 1`) or r2.
    pub fn base_text(&self, p: &str, rev: u64) -> Vec<u8> {
        if rev >= 2 && self.server_modified.contains(p) {
            text(p, None, Some(&server_line(p)))
        } else {
            text(p, None, None)
        }
    }

    /// tpl-r1 working file of `p` given the revision its BASE is at.
    pub fn r1_working(&self, p: &str, rev: u64) -> Vec<u8> {
        let first = self.local_edits.contains(p).then(|| local_line(p));
        let last = if rev >= 2 && self.server_modified.contains(p) {
            Some(server_line(p))
        } else {
            None
        };
        text(p, first.as_deref(), last.as_deref())
    }

    /// Contents on the branch of `p`, a file of [`SWITCH_DIR`] at r1 (named after its path
    /// below `d0`): line 5 changed, and the last line as r2 changes it in `d0`.
    pub fn branch_text(&self, p: &str) -> Vec<u8> {
        let last = self.server_modified.contains(p).then(|| server_line(p));
        text5(p, None, Some(&branch_line(p)), last.as_deref())
    }

    /// tpl-r1 working file of `p` (below [`SWITCH_DIR`]) once switched to the branch.
    pub fn switched_working(&self, p: &str) -> Vec<u8> {
        let first = self.local_edits.contains(p).then(|| local_line(p));
        let last = self.server_modified.contains(p).then(|| server_line(p));
        text5(p, first.as_deref(), Some(&branch_line(p)), last.as_deref())
    }

    /// tpl-r2 working file of edited file `p`.
    pub fn r2_local(&self, p: &str) -> Vec<u8> {
        let last = self.server_modified.contains(p).then(|| server_line(p));
        text(p, Some(&local_line(p)), last.as_deref())
    }
}

fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| err(format!("mkdir: {e}")))?;
    }
    std::fs::write(path, bytes).map_err(|e| err(format!("write {}: {e}", path.display())))
}

fn read(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

fn contains(hay: &[u8], needle: &str) -> bool {
    String::from_utf8_lossy(hay).contains(needle)
}

/// BASE revision of `rel` in `wc.db` (`None`: no BASE row or not present).
fn base_rev(wc: &Path, rel: &str) -> Result<Option<u64>> {
    Ok(base_row(wc, rel)?.and_then(|(_, rev)| rev))
}

/// Repository path and revision of the BASE row of `rel` (`None`: no BASE row or not present).
fn base_row(wc: &Path, rel: &str) -> Result<Option<(String, Option<u64>)>> {
    let c = rusqlite::Connection::open_with_flags(
        wc.join(".svn/wc.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| err(format!("open wc.db: {e}")))?;
    let mut stmt = c
        .prepare(
            "SELECT repos_path, revision FROM nodes WHERE local_relpath = ?1 AND op_depth = 0 \
             AND presence = 'normal'",
        )
        .map_err(|e| err(format!("base_row: {e}")))?;
    let mut rows = stmt
        .query_map([rel], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, Option<i64>>(1)?,
            ))
        })
        .map_err(|e| err(format!("base_row: {e}")))?;
    match rows.next() {
        Some(r) => {
            let (path, rev) = r.map_err(|e| err(format!("base_row: {e}")))?;
            Ok(Some((path, rev.and_then(|v| u64::try_from(v).ok()))))
        }
        None => Ok(None),
    }
}

/// The test fixture: server, svn CLI, repository template and working copy templates.
pub struct Fixture {
    /// The server.
    pub apache: Apache,
    /// The svn CLI.
    pub svn: Svn,
    /// What the repository and templates hold.
    pub model: Model,
    scratch: tempfile::TempDir,
    worker: PathBuf,
    next_id: RefCell<u64>,
    durations: RefCell<HashMap<Op, Duration>>,
    keep_failed: usize,
    kept: RefCell<usize>,
}

impl Fixture {
    /// Starts Apache and builds the fixture with `files` text files. `None` when Apache or svn
    /// is not installed (the suite skips) unless `OPENSVN_REQUIRE_SVN` is on.
    pub fn new(worker: &Path, files: usize, rng: &mut Rng) -> Result<Option<Self>> {
        let Some(svn) = Svn::find()? else {
            return Ok(None);
        };
        if !Apache::available() {
            if std::env::var("OPENSVN_REQUIRE_SVN").is_ok_and(|v| v == "1" || v == "true") {
                return Err(err("apache2 + mod_dav_svn not installed"));
            }
            eprintln!("skipping: apache2 + mod_dav_svn not installed");
            return Ok(None);
        }
        let apache = Apache::start()?;
        let scratch = tempfile::Builder::new()
            .prefix("opensvn-crash-")
            .tempdir()
            .map_err(|e| err(format!("tempdir: {e}")))?;
        let model = Model::build(files, rng);
        let me = Self {
            apache,
            svn,
            model,
            scratch,
            worker: worker.to_path_buf(),
            next_id: RefCell::new(0),
            durations: RefCell::new(HashMap::new()),
            keep_failed: std::env::var("CRASH_KEEP_FAILED")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3),
            kept: RefCell::new(0),
        };
        me.build()?;
        Ok(Some(me))
    }

    fn path(&self, name: &str) -> PathBuf {
        self.scratch.path().join(name)
    }

    fn build(&self) -> Result<()> {
        let m = &self.model;
        self.svn.create_repo(&self.apache.repos_dir().join("tpl"))?;
        self.apache.hand_over("tpl")?;
        let url = self.apache.url("tpl");
        let import = self.path("import");
        for f in &m.files {
            write(&import.join(f), &text(f, None, None))?;
        }
        for (b, bytes) in &m.binary_r1 {
            write(&import.join(b), bytes)?;
        }
        self.svn
            .run(&["import", "-q", "-m", "r1", &import.to_string_lossy(), &url])?;
        // r2: another user's commit.
        let other = self.path("other");
        self.svn
            .run(&["checkout", "-q", &url, &other.to_string_lossy()])?;
        // The branch for `switch`: a copy of d0@1 with line 5 changed (and the last line as in
        // d0@2), plus a file of its own.
        self.svn
            .run_in(&other, &["copy", "-q", SWITCH_DIR, SWITCH_BRANCH])?;
        for f in &m.switched {
            let name = &f[SWITCH_DIR.len() + 1..];
            write(&other.join(SWITCH_BRANCH).join(name), &m.branch_text(f))?;
        }
        let only = format!("{SWITCH_BRANCH}/{BRANCH_ONLY}");
        write(&other.join(&only), b"only on the branch\n")?;
        self.svn.run_in(&other, &["add", "-q", &only])?;
        for f in &m.server_modified {
            write(&other.join(f), &m.base_text(f, 2))?;
        }
        for (b, bytes) in &m.binary_r2 {
            write(&other.join(b), bytes)?;
        }
        let dels: Vec<&str> = m.server_deleted.iter().map(String::as_str).collect();
        if !dels.is_empty() {
            let mut args = vec!["rm", "-q"];
            args.extend(dels);
            self.svn.run_in(&other, &args)?;
        }
        for f in &m.server_added {
            write(&other.join(f), &text(f, None, None))?;
        }
        let mut args = vec!["add", "-q"];
        args.extend(m.server_added.iter().map(String::as_str));
        self.svn.run_in(&other, &args)?;
        self.svn
            .run_in(&other, &["commit", "-q", "-m", "r2 by another user"])?;

        // tpl-r1: local edits at r1 (update / cleanup cases).
        let t1 = self.path("tpl-r1");
        self.svn
            .run(&["checkout", "-q", "-r", "1", &url, &t1.to_string_lossy()])?;
        for f in &m.local_edits {
            write(&t1.join(f), &m.r1_working(f, 1))?;
        }
        for f in &m.conflicts {
            write(&t1.join(f), &text(f, None, Some(&conflict_line(f))))?;
        }
        write(&t1.join("unversioned.txt"), b"not versioned\n")?;

        // tpl-r2: local changes at HEAD (commit / revert cases).
        let t2 = self.path("tpl-r2");
        self.svn
            .run(&["checkout", "-q", &url, &t2.to_string_lossy()])?;
        for f in &m.commit_edits {
            write(&t2.join(f), &m.r2_local(f))?;
        }
        write(&t2.join(&m.commit_binary.0), &m.commit_binary.1)?;
        for f in &m.commit_adds {
            write(&t2.join(f), format!("added {f}\n").as_bytes())?;
        }
        self.svn.run_in(&t2, &["add", "-q", "added"])?;
        let dels: Vec<&str> = m.commit_deletes.iter().map(String::as_str).collect();
        let mut args = vec!["rm", "-q"];
        args.extend(dels);
        self.svn.run_in(&t2, &args)?;
        Ok(())
    }

    /// Runs `crash-worker <state> <op> <url> <wc>` and stops it as `kill` says.
    pub fn worker(
        &self,
        state: &Path,
        op: &str,
        url: &str,
        wc: &Path,
        kill: &Kill,
    ) -> Result<WorkerRun> {
        let mut cmd = Command::new(&self.worker);
        cmd.arg(state)
            .arg(op)
            .arg(url)
            .arg(wc)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_remove(FP_ENV)
            .env_remove(FP_HIT_ENV);
        if let Kill::Failpoint { name, hit } = kill {
            cmd.env(FP_ENV, name).env(FP_HIT_ENV, hit.to_string());
        }
        let start = Instant::now();
        let mut child = cmd
            .spawn()
            .map_err(|e| err(format!("spawn crash-worker: {e}")))?;
        let mut out = child.stdout.take();
        let mut errp = child.stderr.take();
        let t_out = std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(o) = out.as_mut() {
                let _ = o.read_to_string(&mut s);
            }
            s
        });
        let t_err = std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(e) = errp.as_mut() {
                let _ = e.read_to_string(&mut s);
            }
            s
        });
        let limit = Duration::from_secs(600);
        let status = loop {
            if let Some(st) = child.try_wait().map_err(|e| err(format!("wait: {e}")))? {
                break st;
            }
            let el = start.elapsed();
            let due = match kill {
                Kill::After(d) => el >= *d,
                _ => false,
            };
            if due || el >= limit {
                let _ = child.kill(); // SIGKILL
                let st = child.wait().map_err(|e| err(format!("wait: {e}")))?;
                if el >= limit && !due {
                    return Err(err(format!("worker {op} hung for {}s", limit.as_secs())));
                }
                break st;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        let elapsed = start.elapsed();
        let stdout = t_out.join().unwrap_or_default();
        let stderr = t_err.join().unwrap_or_default();
        let ended = if status.signal() == Some(9) {
            Ended::Killed
        } else {
            match stdout.lines().rev().find_map(|l| l.strip_prefix("RESULT ")) {
                Some(r) => match r.strip_prefix("ok") {
                    Some(d) => Ended::Ok(d.trim().to_owned()),
                    None => Ended::Failed(r.trim_start_matches("err").trim().to_owned()),
                },
                None => Ended::Failed(format!("no RESULT line ({status}): {stderr}")),
            }
        };
        Ok(WorkerRun {
            ended,
            elapsed,
            stdout,
            stderr,
        })
    }

    /// Runs the uninterrupted control case of `op` (verified like every other case) and
    /// remembers how long the operation took.
    pub fn control(&self, op: Op) -> CaseRecord {
        self.case(op, &Kill::Never, None)
    }

    /// How long `op` takes uninterrupted (measured by [`Fixture::control`], or by a control
    /// case run now).
    pub fn duration(&self, op: Op) -> Result<Duration> {
        if let Some(d) = self.durations.borrow().get(&op) {
            return Ok(*d);
        }
        let rec = self.control(op);
        self.durations.borrow().get(&op).copied().ok_or_else(|| {
            err(format!(
                "{op:?} never completed uninterrupted: {:?} {:?}",
                rec.ended, rec.failure
            ))
        })
    }

    /// Counts how often failpoint `fp` is hit by an uninterrupted `op`.
    pub fn count_hits(&self, op: Op, fp: &str) -> Result<u64> {
        let rec = self.case(
            op,
            &Kill::Failpoint {
                name: fp.to_owned(),
                hit: 0,
            },
            None,
        );
        match &rec.ended {
            Ended::Ok(_) => {}
            other => {
                return Err(err(format!(
                    "count {op:?}/{fp}: {other:?} {:?}",
                    rec.failure
                )));
            }
        }
        rec.fp_hits.ok_or_else(|| err("no hit count"))
    }

    fn new_id(&self) -> u64 {
        let mut n = self.next_id.borrow_mut();
        *n += 1;
        *n
    }

    /// Runs one case: fresh repository copy and working copy, the worker under `kill`, then
    /// recovery, intermediate checks, completion of the operation and final checks. `pre` is
    /// the delay at which the update preceding a cleanup case is killed (default: half of an
    /// update).
    pub fn case(&self, op: Op, kill: &Kill, pre: Option<Duration>) -> CaseRecord {
        let id = self.new_id();
        let start = Instant::now();
        let mut rec = CaseRecord {
            id,
            op,
            kill: kill.clone(),
            ended: Ended::Failed("not started".into()),
            recovery: None,
            fp_hits: None,
            failure: None,
            warnings: Vec::new(),
            elapsed: Duration::ZERO,
            manual_cleanup: false,
        };
        let dir = self.path(&format!("case-{id}"));
        let repo = format!("r{id}");
        let log_before = self.apache.error_log().len();
        if let Err(e) = self.run_case(op, kill, pre, &dir, &repo, &mut rec) {
            let log = self.apache.error_log();
            let tail: Vec<&str> = log
                .get(log_before..)
                .unwrap_or_default()
                .lines()
                .rev()
                .take(10)
                .collect();
            let mut msg = e.0;
            if !tail.is_empty() {
                msg.push_str("\n  server log: ");
                msg.push_str(
                    &tail
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join("\n  server log: "),
                );
            }
            rec.failure = Some(msg);
        }
        rec.elapsed = start.elapsed();
        let keep = rec.failure.is_some() && *self.kept.borrow() < self.keep_failed;
        if keep {
            *self.kept.borrow_mut() += 1;
            let to = std::env::temp_dir()
                .join(format!("opensvn-crash-failed-{}-{id}", std::process::id()));
            let _ = copy_dir(&dir, &to);
            let _ = copy_dir(&self.apache.repos_dir().join(&repo), &to.join("repo"));
            if let Some(f) = rec.failure.as_mut() {
                f.push_str(&format!(" (kept in {})", to.display()));
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(self.apache.repos_dir().join(&repo));
        rec
    }

    fn run_case(
        &self,
        op: Op,
        kill: &Kill,
        pre: Option<Duration>,
        dir: &Path,
        repo: &str,
        rec: &mut CaseRecord,
    ) -> Result<()> {
        std::fs::create_dir_all(dir).map_err(|e| err(format!("mkdir: {e}")))?;
        self.apache.copy_repo("tpl", repo)?;
        let url = self.apache.url(repo);
        let state = dir.join("state");
        let wc = dir.join("wc");
        match op {
            Op::Checkout | Op::Continue => {}
            Op::Update | Op::Switch | Op::Cleanup => self.clone_template("tpl-r1", &wc, &url)?,
            Op::Commit | Op::Revert => self.clone_template("tpl-r2", &wc, &url)?,
        }
        if op == Op::Cleanup {
            // Leave an interrupted update behind for cleanup to finish.
            let delay = match pre {
                Some(d) => d,
                None => self.duration(Op::Update)? / 2,
            };
            let r = self.worker(&state, "update", &url, &wc, &Kill::After(delay))?;
            if let Ended::Failed(m) = &r.ended {
                return Err(err(format!("pre-cleanup update failed: {m}")));
            }
        }
        if op == Op::Continue {
            // Leave an interrupted checkout behind for Continue to finish. A kill before the
            // working copy database is in place leaves nothing to continue: the checkout is
            // started again, as the app does for such a folder.
            let delay = match pre {
                Some(d) => d,
                None => self.duration(Op::Checkout)? / 2,
            };
            let r = self.worker(&state, "checkout", &url, &wc, &Kill::After(delay))?;
            if let Ended::Failed(m) = &r.ended {
                return Err(err(format!("pre-continue checkout failed: {m}")));
            }
            if !wc.join(".svn/wc.db").is_file() {
                rec.warnings
                    .push("the killed checkout left no working copy; checked out again".into());
                self.worker(&state, "checkout", &url, &wc, &Kill::Never)?
                    .require_ok("checkout again")?;
            }
        }

        // The operation under test.
        let run = self.worker(&state, op.name(), &url, &wc, kill)?;
        rec.ended = run.ended.clone();
        match (&run.ended, kill) {
            (Ended::Failed(m), _)
                if matches!(op, Op::Cleanup | Op::Continue) && m.contains("WcBusy") =>
            {
                // The killed update or checkout left a lock row the engine treats as foreign
                // (see below).
                rec.warnings.push(format!("{} refused: {m}", op.name()));
            }
            (Ended::Failed(m), _) => {
                return Err(err(format!("{} failed without a crash: {m}", op.name())));
            }
            (Ended::Ok(_), Kill::Never) => {
                self.durations.borrow_mut().entry(op).or_insert(run.elapsed);
            }
            (Ended::Ok(_), Kill::Failpoint { hit: 0, .. }) => {
                rec.fp_hits = Some(run.fp_hits().unwrap_or(0));
            }
            _ => {}
        }
        let has_db = wc.join(".svn/wc.db").is_file();

        // 1. Right after the kill: the database is intact.
        if has_db {
            verify::db_integrity(&wc).map_err(|e| err(format!("after kill: {e}")))?;
        }
        // Read before recovery touches `.svn/tmp`.
        let in_lock_window = lock_row_window(kill, &wc);

        // 2. Next open recovers.
        if has_db {
            let r = self.worker(&state, "recover", &url, &wc, &Kill::Never)?;
            let mut detail = r.require_ok("recover")?.to_owned();
            if !detail.contains("locks_need_user=0") {
                // Recovery must need no manual step. The one known exception is a kill right
                // after the WC_LOCK row commits and before its ownership record and marker exist
                // (see `lock_row_window`).
                if !in_lock_window {
                    return Err(err(format!(
                        "recovery needs a manual `svn cleanup` after a kill at {kill}: {detail}"
                    )));
                }
                rec.manual_cleanup = true;
                // A kill between committing the WC_LOCK row and writing the ownership record
                // or marker leaves a row the engine cannot prove is its own; by design
                // (svn-wc `wc_lock.rs`) it is left for the user, whose remedy is `svn cleanup`.
                rec.warnings.push(format!(
                    "lock row without ownership marker after kill; needed `svn cleanup` ({detail})"
                ));
                self.svn
                    .run_in(&wc, &["cleanup"])
                    .map_err(|e| err(format!("svn cleanup of the stuck lock: {e}")))?;
                let r = self.worker(&state, "recover", &url, &wc, &Kill::Never)?;
                detail = r.require_ok("recover after svn cleanup")?.to_owned();
            }
            if !detail.contains("unsettled=0") || !detail.contains("locks_need_user=0") {
                return Err(err(format!("recovery left work for the user: {detail}")));
            }
            rec.recovery = Some(detail);
            verify::db_integrity(&wc).map_err(|e| err(format!("after recovery: {e}")))?;
            verify::db_quiescent(&wc)?;
            if let Err(e) = verify::tmp_clean(&wc) {
                rec.warnings.push(e.0);
            }
            self.status_agrees(&state, &wc, "after recovery")?;
            self.intermediate(op, &wc, &url)?;
        }

        // 3. The user finishes the operation.
        match op {
            Op::Checkout if !has_db => {
                self.worker(&state, "checkout", &url, &wc, &Kill::Never)?
                    .require_ok("checkout after crash")?;
            }
            Op::Checkout | Op::Continue => {
                self.worker(&state, "continue", &url, &wc, &Kill::Never)?
                    .require_ok("continue after crash")?;
            }
            Op::Update | Op::Cleanup => {
                self.worker(&state, "update", &url, &wc, &Kill::Never)?
                    .require_ok("update after crash")?;
            }
            Op::Commit => {
                self.worker(&state, "update", &url, &wc, &Kill::Never)?
                    .require_ok("update before re-commit")?;
                self.worker(&state, "commit", &url, &wc, &Kill::Never)?
                    .require_ok("commit after crash")?;
            }
            Op::Switch => {
                self.worker(&state, "switch", &url, &wc, &Kill::Never)?
                    .require_ok("switch after crash")?;
            }
            Op::Revert => {
                self.worker(&state, "revert", &url, &wc, &Kill::Never)?
                    .require_ok("revert after crash")?;
            }
        }

        // 4. Final state.
        verify::db_integrity(&wc).map_err(|e| err(format!("final: {e}")))?;
        verify::db_quiescent(&wc)?;
        let svn_status = self.status_agrees(&state, &wc, "final")?;
        if op == Op::Switch {
            return self.final_switch(&wc, &url, dir, &svn_status);
        }
        self.final_checks(op, &wc, &url, dir, &svn_status)
    }

    fn clone_template(&self, tpl: &str, wc: &Path, url: &str) -> Result<()> {
        copy_dir(&self.path(tpl), wc)?;
        self.svn.run_in(wc, &["relocate", url])?;
        Ok(())
    }

    /// `svn status` must succeed and agree with the engine's status; returns svn's.
    fn status_agrees(
        &self,
        state: &Path,
        wc: &Path,
        when: &str,
    ) -> Result<BTreeMap<String, String>> {
        let svn = self
            .svn
            .status(wc)
            .map_err(|e| err(format!("{when}: svn cannot read the working copy: {e}")))?;
        let engine = Engine::new(state)?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| err(format!("runtime: {e}")))?;
        let list = rt.block_on(engine.status(wc))?;
        let ours = verify::engine_status_map(&list);
        let diff = verify::status_diff("engine", &ours, "svn", &svn);
        if !diff.is_empty() {
            return Err(err(format!(
                "{when}: engine status disagrees with svn status:\n  {}",
                diff.join("\n  ")
            )));
        }
        Ok(svn)
    }

    /// Checks between recovery and completion: nothing of the user's work is lost, and every
    /// file matches the revision its BASE row claims (no half-applied update, no silent revert).
    fn intermediate(&self, op: Op, wc: &Path, url: &str) -> Result<()> {
        let m = &self.model;
        match op {
            Op::Checkout | Op::Continue => Ok(()),
            Op::Switch => self.intermediate_switch(wc),
            Op::Update | Op::Cleanup => {
                for f in &m.files {
                    let got = read(&wc.join(f));
                    let rev = base_rev(wc, f)?;
                    if m.conflicts.contains(f) {
                        let ok = got
                            .as_deref()
                            .is_some_and(|g| contains(g, &conflict_line(f)));
                        if !ok {
                            return Err(err(format!("lost local modification: {f}")));
                        }
                        continue;
                    }
                    match rev {
                        Some(r) => {
                            let want = m.r1_working(f, r);
                            if got.as_deref() != Some(&want[..]) {
                                return Err(err(format!(
                                    "{f}: BASE is r{r} but the working file is {}",
                                    describe(got.as_deref(), &want, m.local_edits.contains(f))
                                )));
                            }
                        }
                        None => {
                            if !m.server_deleted.contains(f) {
                                return Err(err(format!("{f}: BASE row lost")));
                            }
                            if got.is_some() {
                                return Err(err(format!(
                                    "{f}: deleted by the update but still on disk"
                                )));
                            }
                        }
                    }
                }
                if read(&wc.join("unversioned.txt")).as_deref() != Some(b"not versioned\n") {
                    return Err(err("unversioned file lost"));
                }
                Ok(())
            }
            Op::Commit => {
                let youngest = self.svn.youngest(url)?;
                for f in &m.commit_edits {
                    if read(&wc.join(f)).as_deref() != Some(&m.r2_local(f)[..]) {
                        return Err(err(format!("lost local modification: {f}")));
                    }
                }
                for f in &m.commit_adds {
                    if read(&wc.join(f)).is_none() {
                        return Err(err(format!("lost added file: {f}")));
                    }
                }
                let (b, bytes) = &m.commit_binary;
                if read(&wc.join(b)).as_deref() != Some(&bytes[..]) {
                    return Err(err(format!("lost local modification: {b}")));
                }
                let status = self.svn.status(wc)?;
                match youngest {
                    2 => {
                        // Nothing reached the repository: every change is still local.
                        for f in &m.commit_edits {
                            if status.get(f).map(String::as_str) != Some("M  ") {
                                return Err(err(format!(
                                    "commit did not happen but {f} is {:?}",
                                    status.get(f)
                                )));
                            }
                        }
                    }
                    3 => {
                        // The commit is in the repository: all of it (atomic), and recovery
                        // folded it into the working copy.
                        self.server_has_commit(url)?;
                        if !status.is_empty() {
                            return Err(err(format!(
                                "commit r3 is in the repository but the working copy still shows \
                                 local changes after recovery: {status:?}"
                            )));
                        }
                    }
                    y => return Err(err(format!("unexpected youngest revision r{y}"))),
                }
                Ok(())
            }
            Op::Revert => {
                let status = self.svn.status(wc)?;
                for f in &m.commit_edits {
                    let got = read(&wc.join(f));
                    let base = m.base_text(f, 2);
                    let local = m.r2_local(f);
                    let st = status.get(f).map(String::as_str);
                    let ok = match st {
                        Some("M  ") => got.as_deref() == Some(&local[..]),
                        None => got.as_deref() == Some(&base[..]),
                        _ => false,
                    };
                    if !ok {
                        return Err(err(format!(
                            "{f}: half reverted (status {st:?}, content {})",
                            describe(got.as_deref(), &base, true)
                        )));
                    }
                }
                for f in &m.commit_deletes {
                    let got = read(&wc.join(f));
                    let st = status.get(f).map(String::as_str);
                    let ok = match st {
                        Some("D  ") => got.is_none(),
                        None => got.as_deref() == Some(&m.base_text(f, 2)[..]),
                        _ => false,
                    };
                    if !ok {
                        return Err(err(format!(
                            "{f}: deletion half reverted (status {st:?}, on disk {})",
                            got.is_some()
                        )));
                    }
                }
                for f in &m.commit_adds {
                    if read(&wc.join(f)).is_none() {
                        return Err(err(format!("revert deleted the added file {f}")));
                    }
                }
                Ok(())
            }
        }
    }

    /// A killed switch: every file of `d0` is on the branch (with its local edit merged) or still
    /// at r1 as its BASE row says, every other file is untouched, nothing local is lost.
    fn intermediate_switch(&self, wc: &Path) -> Result<()> {
        let m = &self.model;
        let branch = format!("{SWITCH_BRANCH}/");
        for f in &m.files {
            let got = read(&wc.join(f));
            if m.conflicts.contains(f) {
                if !got
                    .as_deref()
                    .is_some_and(|g| contains(g, &conflict_line(f)))
                {
                    return Err(err(format!("lost local modification: {f}")));
                }
                continue;
            }
            let switched = match base_row(wc, f)? {
                Some((path, _)) => path.starts_with(&branch),
                None => return Err(err(format!("{f}: BASE row lost"))),
            };
            let want = if switched {
                m.switched_working(f)
            } else {
                m.r1_working(f, 1)
            };
            if got.as_deref() != Some(&want[..]) {
                return Err(err(format!(
                    "{f}: BASE is {} but the working file is {}",
                    if switched { "on the branch" } else { "r1" },
                    describe(got.as_deref(), &want, m.local_edits.contains(f))
                )));
            }
        }
        let only = format!("{SWITCH_DIR}/{BRANCH_ONLY}");
        if base_row(wc, &only)?.is_some()
            && read(&wc.join(&only)).as_deref() != Some(b"only on the branch\n")
        {
            return Err(err(format!("{only}: BASE row but wrong contents")));
        }
        if read(&wc.join("unversioned.txt")).as_deref() != Some(b"not versioned\n") {
            return Err(err("unversioned file lost"));
        }
        Ok(())
    }

    /// After a killed and finished switch the working copy equals what svn's own switch makes of
    /// the same template: same files (conflicted files: both sides present), same `svn status`.
    fn final_switch(
        &self,
        wc: &Path,
        url: &str,
        dir: &Path,
        status: &BTreeMap<String, String>,
    ) -> Result<()> {
        let m = &self.model;
        let youngest = self.svn.youngest(url)?;
        if youngest != 2 {
            return Err(err(format!("repository at r{youngest}, expected r2")));
        }
        let oracle = dir.join("oracle");
        self.clone_template("tpl-r1", &oracle, url)?;
        // svn reports the conflict and exits 0 (no `--accept`, non-interactive: postpone).
        self.svn.run_in(
            &oracle,
            &[
                "switch",
                "-q",
                &format!("{url}/{SWITCH_BRANCH}"),
                SWITCH_DIR,
            ],
        )?;
        let conflicted: Vec<&String> = m
            .conflicts
            .iter()
            .filter(|f| m.switched.contains(*f))
            .collect();
        // Conflict artifacts (`f.mine`, `f.r1`, `f.r2`) carry labels; their names are compared
        // through `svn status` only by prefix.
        let artifact = |p: &str| conflicted.iter().any(|f| p.starts_with(&format!("{f}.")));
        let strip = |st: &BTreeMap<String, String>| -> BTreeMap<String, String> {
            st.iter()
                .filter(|(p, c)| !(c.as_str() == "?  " && artifact(p)))
                .map(|(p, c)| (p.clone(), c.clone()))
                .collect()
        };
        let theirs = strip(&self.svn.status(&oracle)?);
        let ours = strip(status);
        if ours != theirs {
            return Err(err(format!(
                "svn status after our switch differs from svn's own switch:\n  {}",
                verify::status_diff("ours", &ours, "svn", &theirs).join("\n  ")
            )));
        }
        let want = snapshot(&oracle)?;
        let got = snapshot(wc)?;
        let mut problems = Vec::new();
        for (p, e) in &want {
            if artifact(p) || conflicted.contains(&p) {
                continue;
            }
            match got.get(p) {
                Some(g) if g == e => {}
                Some(_) => problems.push(format!("{p}: content differs")),
                None => problems.push(format!("{p}: missing")),
            }
        }
        for p in got.keys() {
            if !want.contains_key(p) && !artifact(p) {
                problems.push(format!("{p}: unexpected"));
            }
        }
        for f in &conflicted {
            let g = read(&wc.join(f)).unwrap_or_default();
            if !contains(&g, &conflict_line(f)) || !contains(&g, &server_line(f)) {
                problems.push(format!("{f}: conflict file lacks one side"));
            }
        }
        if !problems.is_empty() {
            problems.truncate(20);
            return Err(err(format!(
                "working copy differs from svn's own switch of the same template:\n  {}",
                problems.join("\n  ")
            )));
        }
        let diff = self.svn.diff(wc)?;
        if let Some(l) = diff
            .lines()
            .find(|l| l.starts_with("-server edit of") || l.starts_with("-branch edit of"))
        {
            return Err(err(format!("silent revert of a branch change: {l}")));
        }
        Ok(())
    }

    /// The repository holds exactly the tpl-r2 changes in r3.
    fn server_has_commit(&self, url: &str) -> Result<()> {
        let m = &self.model;
        for f in &m.commit_edits {
            if self.svn.cat(&format!("{url}/{f}"))? != m.r2_local(f) {
                return Err(err(format!("r3 does not hold the local edit of {f}")));
            }
        }
        for f in &m.commit_adds {
            self.svn
                .cat(&format!("{url}/{f}"))
                .map_err(|e| err(format!("r3 misses added {f}: {e}")))?;
        }
        for f in &m.commit_deletes {
            if self.svn.cat(&format!("{url}/{f}")).is_ok() {
                return Err(err(format!("r3 still has deleted {f}")));
            }
        }
        let (b, bytes) = &m.commit_binary;
        if &self.svn.cat(&format!("{url}/{b}"))? != bytes {
            return Err(err(format!("r3 does not hold the local change of {b}")));
        }
        Ok(())
    }

    fn final_checks(
        &self,
        op: Op,
        wc: &Path,
        url: &str,
        dir: &Path,
        status: &BTreeMap<String, String>,
    ) -> Result<()> {
        let m = &self.model;
        let youngest = self.svn.youngest(url)?;
        let want_youngest = if op == Op::Commit { 3 } else { 2 };
        if youngest != want_youngest {
            return Err(err(format!(
                "repository at r{youngest}, expected r{want_youngest} (lost or duplicated commit)"
            )));
        }
        if op == Op::Commit {
            self.server_has_commit(url)?;
        }
        let fresh = dir.join("fresh");
        self.svn
            .run(&["checkout", "-q", url, &fresh.to_string_lossy()])?;
        let mut want = snapshot(&fresh)?;
        let mut want_status: BTreeMap<String, String> = BTreeMap::new();
        let mut allow_extra: Vec<String> = Vec::new();
        match op {
            Op::Checkout | Op::Commit | Op::Switch | Op::Continue => {}
            Op::Update | Op::Cleanup => {
                for f in &m.local_edits {
                    want.insert(f.clone(), Entry::File(m.r1_working(f, 2)));
                    want_status.insert(f.clone(), "M  ".into());
                }
                for f in &m.conflicts {
                    want.remove(f);
                    want_status.insert(f.clone(), "C  ".into());
                    allow_extra.push(format!("{f}."));
                }
                want.insert(
                    "unversioned.txt".into(),
                    Entry::File(b"not versioned\n".to_vec()),
                );
                want_status.insert("unversioned.txt".into(), "?  ".into());
                // No silent revert: svn diff never removes another user's line.
                let diff = self.svn.diff(wc)?;
                if let Some(l) = diff.lines().find(|l| l.starts_with("-server edit of")) {
                    return Err(err(format!("silent revert of a server change: {l}")));
                }
            }
            Op::Revert => {
                want.insert("added".into(), Entry::Dir);
                for f in &m.commit_adds {
                    want.insert(f.clone(), Entry::File(format!("added {f}\n").into_bytes()));
                }
                want_status.insert("added".into(), "?  ".into());
            }
        }
        // Conflict artifacts (`f.mine`, `f.r1`, `f.r2`) are unversioned files for svn too.
        let status: BTreeMap<String, String> = status
            .iter()
            .filter(|(p, c)| {
                !(c.as_str() == "?  " && allow_extra.iter().any(|a| p.starts_with(a.as_str())))
            })
            .map(|(p, c)| (p.clone(), c.clone()))
            .collect();
        let status = &status;
        if status != &want_status {
            return Err(err(format!(
                "final svn status differs from the expected one:\n  {}",
                verify::status_diff("svn", status, "expected", &want_status).join("\n  ")
            )));
        }
        let got = snapshot(wc)?;
        let mut problems = Vec::new();
        for (p, e) in &want {
            match got.get(p) {
                Some(g) if g == e => {}
                Some(_) => problems.push(format!("{p}: content differs")),
                None => problems.push(format!("{p}: missing")),
            }
        }
        for p in got.keys() {
            if !want.contains_key(p)
                && !m.conflicts.contains(p)
                && !allow_extra.iter().any(|a| p.starts_with(a.as_str()))
            {
                problems.push(format!("{p}: unexpected"));
            }
        }
        for f in &m.conflicts {
            if op == Op::Update || op == Op::Cleanup {
                let g = read(&wc.join(f)).unwrap_or_default();
                if !contains(&g, &conflict_line(f)) || !contains(&g, &server_line(f)) {
                    problems.push(format!("{f}: conflict file lacks one side"));
                }
            }
        }
        if !problems.is_empty() {
            problems.truncate(20);
            return Err(err(format!(
                "final working copy differs from a fresh svn checkout + local changes:\n  {}",
                problems.join("\n  ")
            )));
        }
        Ok(())
    }
}

fn describe(got: Option<&[u8]>, want: &[u8], local: bool) -> String {
    match got {
        None => "missing".into(),
        Some(g) if g == want => "as expected".into(),
        Some(g) => {
            let s = String::from_utf8_lossy(g);
            let first = s.lines().next().unwrap_or_default().to_owned();
            let last = s.lines().last().unwrap_or_default().to_owned();
            format!(
                "different (first line {first:?}, last line {last:?}, local edit expected: {local}, \
                 expected {:?})",
                String::from_utf8_lossy(want)
                    .lines()
                    .last()
                    .unwrap_or_default()
            )
        }
    }
}

/// Aggregated results of a suite run.
#[derive(Debug, Default)]
pub struct Report {
    /// Seed of the run.
    pub seed: u64,
    /// Every case.
    pub cases: Vec<CaseRecord>,
    /// Failpoint coverage: `(op, failpoint) -> hits of an uninterrupted run`.
    pub coverage: BTreeMap<(Op, String), u64>,
}

impl Report {
    /// Cases that failed.
    pub fn failures(&self) -> Vec<&CaseRecord> {
        self.cases.iter().filter(|c| c.failure.is_some()).collect()
    }

    /// Markdown summary.
    pub fn markdown(&self, title: &str) -> String {
        let mut s = format!(
            "# {title}\n\nSeed: `{}` (replay with `CRASH_SEED={}`)\n\n",
            self.seed, self.seed
        );
        s.push_str(
            "| op | cases | killed | completed before kill | manual cleanup | failed |\n|---|---:|---:|---:|---:|---:|\n",
        );
        for op in Op::ALL {
            let cs: Vec<_> = self.cases.iter().filter(|c| c.op == op).collect();
            if cs.is_empty() {
                continue;
            }
            let killed = cs.iter().filter(|c| c.ended == Ended::Killed).count();
            let done = cs
                .iter()
                .filter(|c| matches!(c.ended, Ended::Ok(_)))
                .count();
            let failed = cs.iter().filter(|c| c.failure.is_some()).count();
            let manual = cs.iter().filter(|c| c.manual_cleanup).count();
            s.push_str(&format!(
                "| {} | {} | {killed} | {done} | {manual} | {failed} |\n",
                op.name(),
                cs.len()
            ));
        }
        if !self.coverage.is_empty() {
            s.push_str("\n## Failpoint coverage\n\nOperations that never reach a failpoint are left out.\n\n| failpoint | op | hits per run | kills | failed |\n|---|---|---:|---:|---:|\n");
            for ((op, fp), hits) in self.coverage.iter().filter(|(_, h)| **h > 0) {
                let mine: Vec<_> = self
                    .cases
                    .iter()
                    .filter(|c| {
                        c.op == *op
                            && matches!(&c.kill, Kill::Failpoint { name, hit } if name == fp && *hit > 0)
                    })
                    .collect();
                let kills = mine.iter().filter(|c| c.ended == Ended::Killed).count();
                let failed = mine.iter().filter(|c| c.failure.is_some()).count();
                s.push_str(&format!(
                    "| `{fp}` | {} | {hits} | {kills} | {failed} |\n",
                    op.name()
                ));
            }
            let never: Vec<&str> = FAILPOINTS
                .iter()
                .copied()
                .filter(|fp| !self.coverage.iter().any(|((_, f), h)| f == fp && *h > 0))
                .collect();
            if !never.is_empty() {
                s.push_str(&format!(
                    "\nNever reached by any operation: {}\n",
                    never
                        .iter()
                        .map(|f| format!("`{f}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        let warn: BTreeSet<&str> = self
            .cases
            .iter()
            .flat_map(|c| c.warnings.iter().map(String::as_str))
            .collect();
        if !warn.is_empty() {
            s.push_str(&format!("\n## Warnings ({} distinct)\n\n", warn.len()));
            for w in warn.iter().take(20) {
                s.push_str(&format!("- {w}\n"));
            }
        }
        let fails = self.failures();
        s.push_str(&format!("\n## Failures: {}\n\n", fails.len()));
        for c in fails.iter().take(50) {
            s.push_str(&format!(
                "- case {} `{}` kill `{}` ended {:?}, recovery {:?}:\n  ```\n  {}\n  ```\n",
                c.id,
                c.op.name(),
                c.kill,
                c.ended,
                c.recovery,
                c.failure
                    .as_deref()
                    .unwrap_or_default()
                    .replace('\n', "\n  ")
            ));
        }
        s
    }

    /// Writes `<name>.md` to `CRASH_REPORT_DIR` (default `target/crash-report`).
    pub fn write(&self, name: &str, title: &str) -> Result<PathBuf> {
        let dir = std::env::var_os("CRASH_REPORT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/crash-report")
            });
        std::fs::create_dir_all(&dir).map_err(|e| err(format!("mkdir report: {e}")))?;
        let path = dir.join(format!("{name}.md"));
        std::fs::write(&path, self.markdown(title))
            .map_err(|e| err(format!("write report: {e}")))?;
        Ok(path)
    }
}

/// Reads a count from the environment.
pub fn env_count(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
