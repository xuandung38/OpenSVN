# OpenSVN crash suite

Shows that a process killed in the middle of a working copy operation never corrupts the working
copy, never loses a local change and never silently reverts a server change, and that the next
open recovers. Linux only.

## What runs

- A throwaway **Apache httpd + mod_dav_svn** on `127.0.0.1` (plain HTTP, anonymous read/write,
  `SVNParentPath` in a temp dir). Started per test run (`src/server.rs`).
- A fixture repository built with the **svn CLI**: r1 = 200 text files in 10 folders plus two
  1 MiB binaries; r2 = another user's commit (last line of every third file, deletes, adds, a
  binary delta). Two template working copies:
  - `tpl-r1`: at r1, first line of every fourth file edited locally (some of them also changed in
    r2, so they are clean merges), two files edited on the last line (text conflicts with r2), and
    one unversioned file. Used by `update` and `cleanup`: "mid-update with local edits".
  - `tpl-r2`: at HEAD, local edits, a binary rewritten locally, an added folder and deleted files.
    Used by `commit` and `revert`.
  - r2 also branches `d0` to `d0-branch` (a copy of `d0@1`): every file gets a line-5 change and
    r2's last-line change, and the branch has one file of its own. `switch` switches `d0` of
    `tpl-r1` to the branch, so the local edits merge and one file conflicts.
- **`crash-worker`** (`src/bin/crash-worker.rs`): a separate process that runs one operation
  through the real engine (`svn-ra` HTTP + `svn-client` + `svn-wc`), exactly as the app wires it.
  Ops: `checkout`, `update`, `switch`, `commit`, `revert`, `cleanup`, and `recover`. `recover` is what the app
  runs when it opens a working copy: release stale locks of dead processes, run the unfinished work
  queue, settle commit journals. The lock owner registry is a file with the app's liveness rules
  (pid + `/proc` start time + boot id), so a later process can tell that a killed one is dead.

Every case uses its own copy of the repository and of the template:

1. Run the operation in a worker, and **kill it with SIGKILL**:
   - `tests/sigkill.rs`: at a uniformly random moment of the operation's measured duration (x1.1,
     so some runs complete);
   - `tests/failpoints.rs` (feature `failpoints`): at hit *k* of a named failpoint of `svn-wc` /
     `svn-client` / `svn-ra`. Every failpoint gets `CRASH_FP_ITERS` kills, spread over the
     operations that reach it. The worker arms the `fail` crate with a callback that sends itself
     `SIGKILL`.
     First, an uninterrupted run counts the hits. Then the worker is killed at the first hit, the
     last hit and random hits in between. Examples: `commit.after_merge_before_finalize` is
     "after MERGE"; `update.after_txn_before_queue` #k is "mid-update with local edits" after k
     files; `net.update.mid_stream` #k is a checkout or update killed while the REPORT is still
     streaming in, after its k-th chunk; `wc.create.*` is a checkout killed while it builds `wc.db`; `wc.txn.*`, `wc.wq.*`,
     `pristine.install.before_rename` cover every database transaction, work item and pristine
     install.
2. Right after the kill: `PRAGMA integrity_check` and `foreign_key_check`, no NODES row without a
   parent, and every referenced pristine exists.
3. **Next open recovers**: `crash-worker recover` must succeed and leave no work item, no lock row
   and no unsettled commit journal. `svn status` (real svn) must read the working copy, and agree
   with the engine's status (item, property and tree-conflict columns).
4. **Intermediate checks** (before the user finishes the operation):
   - update/cleanup: every text file matches the revision its BASE row claims, with the local edit
     still on line 1. No half-applied file, no BASE moved forward without the server change on
     disk, no local edit lost. The conflict files still hold the local line, and the unversioned
     file is untouched.
   - commit: the repository is at r2 (nothing committed, every change still local) or at r3 with
     **all** changes (atomic). In that case recovery folded it into the working copy, and status is
     clean. Local contents are never lost.
   - revert: every file is either fully reverted or still fully modified, and status agrees.
5. The user finishes the operation (re-run checkout, update, update + commit, revert). It must
   succeed.
6. **Final checks**: the repository is at the expected revision (no lost or duplicated commit). The
   working copy equals a fresh `svn checkout` of HEAD plus the expected local changes, file by
   file. `svn status` equals the expected status (and the engine's). `svn diff` never removes a
   line of another user's commit (no silent revert).

A control case (no kill) runs for every operation first.

## Run

```sh
sudo apt-get install subversion apache2 libapache2-mod-svn
cargo test -p opensvn-crash --test sigkill -- --ignored --nocapture
cargo test -p opensvn-crash --features failpoints --test failpoints -- --ignored --nocapture
```

| Variable | Default | Meaning |
|---|---|---|
| `CRASH_RANDOM_ITERS` | 6 | random kills per operation |
| `CRASH_FP_ITERS` | 50 | kills per reachable failpoint, spread over the operations that reach it |
| `CRASH_FILES` | 200 | text files in the fixture |
| `CRASH_SEED` | clock | replay a run (printed at start and in the report) |
| `CRASH_REPORT_DIR` | `target/crash-report` | markdown reports (`sigkill.md`, `failpoints.md`) |
| `CRASH_KEEP_FAILED` | 3 | failed cases copied to `/tmp/opensvn-crash-failed-*` (WC, state, repo) |
| `OPENSVN_REQUIRE_SVN` | off | fail instead of skipping when svn or Apache is missing |
| `OPENSVN_APACHE_BIN`, `OPENSVN_APACHE_MODULES` | auto | non-Debian layouts |

The tests are `#[ignore]` so that `cargo test --workspace` stays fast and needs no server. The
nightly workflow (`.github/workflows/nightly.yml`) runs over 1000 cases.

The crate's `failpoints` feature is off by default. Turning it on enables `svn-client/failpoints`
and `svn-ra/failpoints`,
and keeping it off means `cargo test --workspace` never compiles the engine with failpoints through
feature unification.

### Switch

`switch` cases check, between recovery and completion, that every file of `d0` is either on the
branch (with its local edit merged) or still at r1, as its BASE row says, and that nothing
outside `d0` moved. After the user re-runs the switch, the working copy must equal what svn's own
`svn switch` makes of the same template: same files (a conflicted file must hold both sides),
same `svn status`, and `svn diff` never removes a branch or server line.

### Recovery needs no manual step

Recovery (`crash-worker recover`, what the app runs when it opens a working copy) must leave no
lock row, work item or unsettled commit journal for the user. There is one exception, by design
of `svn-wc` `wc_lock.rs`: the WC_LOCK row is committed before its ownership record and marker are
written, so that a crash never makes a foreign lock look like ours. A kill in that window leaves
a lock row nobody can prove is ours, and the user has to run `svn cleanup`. The suite allows it
only for a random SIGKILL or a kill at `wc.txn.after_commit` (any hit: for `continue` the lock
is not the first transaction) that left no lock marker in `.svn/tmp`, counts it in the "manual
cleanup" column, and fails any other case that needs it.

## Concurrency: engine and svn on one working copy

`tests/concurrency.rs` (svn 1.15 required) pairs an engine operation with an svn command on one
working copy at the same time: update vs status, status vs update, update vs update, commit vs
cleanup, cleanup vs commit, commit vs commit, update vs commit, commit vs update, commit vs status,
cleanup vs update. Each side may only fail because of the other's lock: the engine with
`ErrorCode::WcBusy` (any other code fails, and an untyped "locked"/"busy" error is named as such),
svn with E155004, E200033 or E155037 (svn looks for queued work before it reads the lock rows,
so a writer holding the lock with work queued reads as "previous operation has not finished").
Afterwards `wc.db` is intact and quiescent, both statuses agree, every edit is in the
repository and the working copy equals a fresh checkout. `locks_are_honoured_both_ways` holds the
engine's lock while svn writes (E155004 each time; `svn status` works) and puts svn's lock row in
`wc.db` while the engine writes (`WcBusy`, nothing changes; status works).

`svn cleanup` always breaks locks (`cleanup-cmd.c`: `break_locks = TRUE`), runs the queued work,
vacuums unreferenced pristines and empties `.svn/tmp`. `engine_update_vs_svn_cleanup` runs an
engine update (alternately to r1 and HEAD) against a loop of `svn cleanup` with stricter checks:
the update finishes, finds the working copy busy, or stops with the typed "working copy was
cleaned up by another client" error (`WcBusy`) having written nothing more. It never fails on a
vanished temporary file. Afterwards no NODES row references a missing pristine and a later update
finishes the interrupted one. The report counts the stopped updates in its fifth engine column.

```sh
cargo test -p opensvn-crash --test concurrency -- --ignored --nocapture
```

## Soak

`tests/soak.rs`: one engine process runs update, status, commit, status and `log -l 100 -v` in a
loop while svn commits as another user, for 2 hours (`soak_2h`) or 2 minutes (`soak_short`), and
the RSS at the end must be less than 10% above the RSS after the warm-up (the first 10% of the
run). Every text keeps its size from cycle to cycle, so only memory the engine keeps can grow.
`SOAK_SECS` overrides the duration.

```sh
cargo test --release -p opensvn-crash --test soak -- --ignored --nocapture soak_2h
```

## Known limits

- The nightly workflow uses svn 1.14.3 (Ubuntu 24.04 package) as oracle and server; the
  concurrency suite needs svn 1.15 and runs locally (`OPENSVN_SVN_BIN`, `OPENSVN_APACHE_BIN`,
  `OPENSVN_APACHE_MODULES` point at a 1.15 build, for instance one built with nix).
- The "real app" variant (tauri-driver) is not part of this crate yet.
