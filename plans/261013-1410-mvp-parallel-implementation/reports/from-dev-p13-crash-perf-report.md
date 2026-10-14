# P13 crash suite and benchmarks: report

Branch `feat/p13-crash-perf`, PR #117 (Refs #21). This report covers the automated part of P13:
the crash suite, the benchmarks against the svn CLI, and the nightly workflow. Production code is
unchanged. The failpoints that already exist in `svn-wc` / `svn-client` were enough; this PR adds
none.

## What was built

- `tests/crash/` (crate `opensvn-crash`, Linux only):
  - Throwaway Apache + mod_dav_svn on 127.0.0.1.
  - Fixture repository with r1 (the base files) and r2 (another user's commit), and two template
    working copies:
    - `tpl-r1`: local edits, clean merges, two text conflicts, one unversioned file.
    - `tpl-r2`: edits, adds, deletes, one rewritten binary.
  - Worker process `crash-worker`. It runs checkout, update, commit, revert, cleanup or the app's
    recovery through `svn-ra` + `svn-client` + `svn-wc`.
  - SIGKILL modes:
    - random moment: `tests/sigkill.rs`;
    - hit *k* of one of the 15 named failpoints: `tests/failpoints.rs`, feature `failpoints`. The
      worker kills itself from a `fail::cfg_callback`.
  - Verification, run with the real `svn` as oracle:
    - wc.db integrity (SQLite checks, NODES parent, pristines present);
    - recovery leaves no work item, lock row or unsettled journal;
    - `svn status` equals the engine status;
    - every file matches the revision of its BASE row, and local edits are kept;
    - commit is atomic and folded into the working copy;
    - revert has no half-reverted file;
    - after finishing the operation, the working copy equals a fresh `svn checkout` plus the
      expected local changes;
    - the repository revision is the expected one;
    - `svn diff` never removes another user's line.
  - Markdown report with the failpoint coverage table. See `tests/crash/README.md`.
- `benches/` (crate `opensvn-bench`, binary `compare-svn`):
  - Generated repository: 100k text files + 4 x 64 MiB incompressible binaries, then r2 changes
    10k files and one binary.
  - Timed operations: checkout, update r1 -> r2, and status (first run with the page cache
    dropped, repeat runs). Both the engine and svn run as separate processes against the same
    server.
  - Each engine result is checked with `svn status` / `svn info`.
  - Report written as markdown + JSON. See `benches/README.md`.
  - Measured results: `benches/results/2026-10-14-100k-local-vm.md`.
- `.github/workflows/nightly.yml`:
  - crash job: 600 random cases + about 40 x 8 failpoint kills, so more than 1000 cases a night.
    Kept failing working copies are uploaded as artifacts.
  - bench job: 100k files, 2 repetitions, a warning when the engine is more than 10% slower.
  - Reports go to the job summary and to artifacts.
  - actionlint is clean.

## Results

### Benchmarks (release, svn 1.14.3, 4 vCPU VM): both MVP gates fail

| operation | svn CLI | engine | engine / svn |
|---|---:|---:|---:|
| checkout HEAD (100k files, 452 MiB) | 36.65 s | 947.39 s | 25.9 |
| update r1 -> r2 (10k files + 1 binary) | 8.97 s | 82.88 s | 9.2 |
| status, first run (page cache dropped) | 1.25 s | 17.08 s | 13.7 |
| status, repeat run | 0.43 s | 14.68 s | 34.0 |

- "Not slower than svn CLI": **does not hold** for any operation.
- "Repeat status < 3 s on 100k files": **does not hold** (14.68 s).
- Causes, with strace profiles, are in #121:
  - checkout/update: about 19 fsync per file and about 4 wc.db write transactions per node, with
    `synchronous = FULL`;
  - status: about 12 SQLite read transactions and 12 failed stats per node.

### Crash suite

CRASH_RESULTS_PLACEHOLDER

## Issues opened (no production fix in this PR)

- #119: the update reporter does not report the children of an incomplete (start-empty)
  directory, so an interrupted update can never be resumed (server: "Working copy path 'bin' does
  not exist in repository"). This is the cause of every update/cleanup crash-case failure. A
  one-line fix is suggested and was verified in a separate worktree.
- #120: work-queue temp files `wq-*` are created in user folders and left there after a kill
  (they show as `?`); `.svn/tmp` leftovers are never cleaned.
- #121: performance, numbers above.

Observed and not filed, because it is documented in `svn-wc/src/db/wc_lock.rs`: a kill between
committing the WC_LOCK row and writing the ownership record or marker leaves a lock the engine
treats as foreign. The next open then needs the user (`svn cleanup`). The suite counts these cases
as warnings and resolves them with real `svn cleanup` before verifying.

## Not covered (still open in P13)

- svn 1.15.0 as oracle and baseline. Ubuntu 24.04 ships 1.14.3; the 1.15 builds exist only in the
  Docker compat images.
- Crash variant through the real app (tauri-driver), the concurrency suite (engine + svn CLI on
  one working copy), compat full matrix, security suite, fuzz, 2 h soak, app RAM, log 100k
  revisions and commit 1k files benchmarks, dogfood.
- `switch` in the crash suite.

## Gates

GATES_PLACEHOLDER
