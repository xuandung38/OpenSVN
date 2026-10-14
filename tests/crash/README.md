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
- **`crash-worker`** (`src/bin/crash-worker.rs`): a separate process that runs one operation
  through the real engine (`svn-ra` HTTP + `svn-client` + `svn-wc`), exactly as the app wires it.
  Ops: `checkout`, `update`, `commit`, `revert`, `cleanup`, and `recover`. `recover` is what the app
  runs when it opens a working copy: release stale locks of dead processes, run the unfinished work
  queue, settle commit journals. The lock owner registry is a file with the app's liveness rules
  (pid + `/proc` start time + boot id), so a later process can tell that a killed one is dead.

Every case uses its own copy of the repository and of the template:

1. Run the operation in a worker, and **kill it with SIGKILL**:
   - `tests/sigkill.rs`: at a uniformly random moment of the operation's measured duration (x1.1,
     so some runs complete);
   - `tests/failpoints.rs` (feature `failpoints`): at hit *k* of a named failpoint of `svn-wc` /
     `svn-client`. The worker arms the `fail` crate with a callback that sends itself `SIGKILL`.
     First, an uninterrupted run counts the hits. Then the worker is killed at the first hit, the
     last hit and random hits in between. Examples: `commit.after_merge_before_finalize` is
     "after MERGE"; `update.after_txn_before_queue` #k is "mid-update with local edits" after k
     files; `wc.txn.*`, `wc.wq.*`, `pristine.install.before_rename` cover every database
     transaction, work item and pristine install.
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
| `CRASH_FP_ITERS` | 3 | kills per reachable (operation, failpoint) |
| `CRASH_FILES` | 200 | text files in the fixture |
| `CRASH_SEED` | clock | replay a run (printed at start and in the report) |
| `CRASH_REPORT_DIR` | `target/crash-report` | markdown reports (`sigkill.md`, `failpoints.md`) |
| `CRASH_KEEP_FAILED` | 3 | failed cases copied to `/tmp/opensvn-crash-failed-*` (WC, state, repo) |
| `OPENSVN_REQUIRE_SVN` | off | fail instead of skipping when svn or Apache is missing |
| `OPENSVN_APACHE_BIN`, `OPENSVN_APACHE_MODULES` | auto | non-Debian layouts |

The tests are `#[ignore]` so that `cargo test --workspace` stays fast and needs no server. The
nightly workflow (`.github/workflows/nightly.yml`) runs over 1000 cases.

The crate's `failpoints` feature is off by default. Turning it on enables `svn-client/failpoints`,
and keeping it off means `cargo test --workspace` never compiles the engine with failpoints through
feature unification.

## Known limits

- svn 1.14.3 (Ubuntu 24.04 package) is the oracle and the server. The 1.15.0 builds live in the
  Docker compat images (`docker/`), and this suite does not use them yet.
- The "real app" variant (tauri-driver) and the concurrency suite (engine and svn CLI on one
  working copy) are not part of this crate yet.
- `switch` is not exercised.
