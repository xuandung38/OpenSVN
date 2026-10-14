# Status obstructions, wc.db busy, file→dir update replacement (#143–#146), PR #147

Branch `fix/status-obstructions`, from origin/main `2453236`. Merged origin/main `aaa9269`
(#139 + #140) with merge commit `8b2e032`. Merge commits only.

Oracle: svn 1.15.0 built from the repo's pinned recipe: the version and SHA-512 from
`docker/compose.yml`, the configure flags from `docker/svn-client/Dockerfile`, installed in
`/opt/svn15`. Every gate ran with `OPENSVN_SVN_BIN=/opt/svn15/bin/svn OPENSVN_REQUIRE_SVN=1` and
`CARGO_INCREMENTAL=0`. No oracle test was skipped.

## 1. Versioned directory replaced by a file or symlink (#143)

Before, the whole status failed with `UnsafePath`: the walk's `safe_join` refused a non-directory
or a symlink as an intermediate component.

Now:
- `local_status` reaches each directory with `reach()`. It `lstat`s every component from the
  working copy root, and it stops looking at the disk below the first component that is not a
  real directory: a file, nothing, or a symlink. A symlink is never followed. Names are still
  checked like `safe_join` (admin-dir aliases, plain components).
- A directory that is not a real directory is not listed (`read_dir` runs only on real
  directories), and its versioned children are not `lstat`ed. They are reported missing (`!`).
  The node itself is obstructed (`~`).
- The status target is resolved lexically, as svn does it (`wc::open_target`, used by
  `svn_client::status` only). A target below an obstruction (`d/sub`), and a missing target,
  now give svn's answer instead of the `canonicalize` error. If no working copy is found
  lexically, or the path contains `..`, the old `open_containing` path is used.

Matches svn 1.15 for a directory replaced by a file: the target itself, targets below it, and
the default, `--no-ignore` and `--depth immediates` option sets.

**Deliberate difference:** svn follows a symlink that replaced a versioned directory, even when
it points out of the working copy, and reports what it finds behind it (normal, modified and
unversioned items). We report `~` for the link and `!` for its versioned children, and we
never read behind the link. The test asserts this, and also asserts that svn shows `~`. The
rest of the working copy matches svn exactly.

## 2. svn:special link ↔ regular file (#144)

svn's `assemble_status` (`HAVE_SYMLINK` builds) compares `svn:special` in the working
properties with "is a symlink on disk". On a mismatch it reports `~` and does not compare the
texts. `node_status` now does the same (`special_mismatch`, Unix only; Windows keeps regular
files, like svn without symlinks). Covered and identical to svn 1.15:
- link → regular file
- file → link
- `propset`/`propdel svn:special` (`~M`)
- added link/file swapped
- retargeted link stays `M`

## 3. WcBusy under a concurrent writer (#145)

Two causes, both fixed. wc.db stays in rollback-journal mode (no WAL, format 31 unchanged).
1. **Write lock on open.** `open_and_recover` → `wc_cleanup_locks` always ran a
   `BEGIN IMMEDIATE` transaction, even with nothing to remove. A status opened while another
   process held RESERVED waited 10 s and failed `WcBusy`. Now it writes only when there is a stale
   lock to delete. Regression test `status_does_not_wait_for_the_write_lock_like_svn`: it holds
   RESERVED from another connection. Before the fix: `WcBusy` after 10.2 s. After: immediate. svn
   status succeeds in the same state.
2. **Deadlock in the split read (#139).** `read_subtree` holds SHARED on the main connection while
   helper connections read rowid ranges. If a writer commits in between, it takes PENDING and waits
   for that SHARED lock. The helpers cannot take a new SHARED lock past PENDING, so both sides
   waited out the 10 s busy timeout. Helpers now use busy timeout 0. A busy helper hands its range
   back to the main connection, which still holds SHARED and can read. The writer commits right
   after. The read stays one consistent snapshot. Deterministic regression test
   `svn-wc/tests/subtree_busy_failpoints.rs` (new failpoint `wc.subtree.helper_before_read`,
   `sleep`): 11.6 s and failure without the fix, 1.9 s with it.

Stress (release build, 10k files, writer = svn `update -r1/-r2` + `propset/propdel` in a loop,
300 status runs each): see the table in "Benchmark".

## 4. Incoming file→directory replacement during update (#146)

Reproduced against svn 1.15 for six local states of the file:
- clean
- edited
- property-edited
- `svn rm`ed
- missing
- replaced by an unversioned directory

The reverse direction (directory→file) was reproduced for six states too:
- clean
- child edited
- unversioned child
- `svn rm`ed
- missing
- replaced by an unversioned file

The tests compare the working files, the NODES and ACTUAL rows (conflict skels included) and
`svn status`.

- The repro posted on #146 (unmodified file) came from the oracle test helper: `parse_dump`
  ignored `Node-action: replace`, so the fake repository never held the directory. The helper now
  emits `Delete` before the add.
- Engine fix, svn's `deletion_conflicts`: when `delete_entry` raised a tree conflict and the drive
  adds the same name, the add is a replacement. The conflict becomes "local <reason>, incoming
  replace". The left side is the deleted node and the right side is the added one. The new node
  goes into BASE, shadowed. `base-deleted` rows are written only where a local working layer
  exists. Op depth 0 means "BASE only", for a node deleted locally. The disk is never touched.
  This applies to both `add_directory` and `add_file`.
- An obstructed node (another kind on disk, or a special/symlink mismatch) now counts as a local
  change for an incoming delete, as in svn's `modcheck_callback`. The local item is kept with a
  tree conflict. Before, the update tried to remove it and failed.
- The restore pass before the drive no longer descends below a directory replaced by a file or
  symlink. Before, it failed with `UnsafePath`.

All twelve variants match svn 1.15. The two tests fail on main (NODES rows differ).

## Gates (head after merging main)

| gate | result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo clippy -p opensvn --features real-engine --all-targets -- -D warnings` | clean |
| clippy with failpoints (svn-wc, svn-client, opensvn-crash, opensvn) | clean |
| `cargo test -p svn-wc --features failpoints` | 247 passed, 0 failed, 1 ignored |
| `cargo test -p svn-client --features failpoints` | 224 passed, 0 failed |
| `cargo test -p opensvn --features failpoints` | 493 passed, 0 failed |
| `cargo test --workspace --locked` (svn 1.15 oracle, `OPENSVN_REQUIRE_SVN=1`) | 1429 passed, 0 failed, 22 ignored |
| `cargo test -p opensvn --features opensvn/real-engine` | 487 passed, 0 failed |
| crash suite, sigkill (Apache + mod_dav_svn) | 35 cases, 0 failed |
| crash suite, named failpoints | 90 cases, 0 failed |

## Benchmark (release build, same 4-vCPU VM, svn 1.15.0)

`compare-svn --files 20000 --binaries 1 --binary-mb 16 --update-files 2000 --reps 3` (Apache +
mod_dav_svn on 127.0.0.1, the #139 configuration). The table shows the median, with the minimum
in brackets. Each run measures svn as well.

| status | main `aaa9269` (engine / svn) | this branch (engine / svn) |
|---|---:|---:|
| first run, page cache dropped | 0.13 s / 0.37 s | 0.13 s / 0.32 s |
| repeat run (svn's checkout) | 0.07 s / 0.07 s | 0.06 s / 0.06 s |
| repeat run, own checkout | 0.05 s / 0.05 s | 0.07 s / 0.07 s ¹ |

¹ This row moved for svn too, so it is noise. The bench rounds to 10 ms.

21 interleaved runs (median, min) of `compare-svn engine status` on one 10k-file working copy:
- main: 26.6 ms (24.4)
- branch: 26.0 ms (24.8)
- svn: 24.9 ms (24.3)

There is no status regression.

Stress (#145): 300 status runs on a 10k-file working copy for each configuration below, with a
concurrent writer. Two writers were used:
- (a) svn `update -r1/-r2` + `propset/propdel` in a loop;
- (b) a sqlite connection doing `BEGIN IMMEDIATE`, an update, a 30 ms hold and `COMMIT`, in a
  loop.

| status of | writer (a) | writer (b) |
|---|---:|---:|
| main | 0 / 300 failed | 0 / 300 failed |
| branch | 0 / 300 failed | 0 / 300 failed |
| svn 1.15 | 0 / 300 failed | 0 / 300 failed |

This harness did not reproduce the 4/300 seen on main, so the timing window was not hit here.
The two deterministic regression tests above do reproduce both causes on main:
- `WcBusy` after 10.2 s;
- 11.6 s deadlock, then failure.

Both pass on the branch. No run took more than 5 s.


## Adversarial self-review

- **Data loss.**
  - The status changes only read.
  - The update changes keep more local data, never less: an obstruction is now a local change,
    so it becomes a tree conflict instead of a removal.
  - An incoming replacement over a conflict never touches the disk.
  - On main, the edited-file case kept the user's file but left BASE stale. The unversioned-dir
    case failed the update without losing data.
- **Path escape.**
  - Nothing below a non-directory or a symlink is `lstat`ed, listed or opened.
  - The target lookup is lexical, so a target below a symlink is reported missing, not resolved
    through the link. A symlink to another working copy root is opened as that working copy,
    which is what svn and the previous `canonicalize` did.
  - Unchanged and known: the time-of-check gap between `lstat` and `read_dir`/`open` (a
    directory swapped for a symlink in that window) existed before this change. It only affects
    reads.
- **Status performance.** `reach()` costs the same `lstat`s per directory job as the previous
  `abs_path` (`safe_join`). `special_mismatch` adds one in-memory property lookup per file. See
  the benchmark.
- **Concurrency.** Helpers never wait. While the main connection holds SHARED no writer can
  commit, so a part read on a helper or on the main connection comes from the same snapshot.

## Not changed (follow-ups)

- `status_remote`, update and the other commands still resolve their target with
  `open_containing` (`canonicalize`). Only local status uses the lexical lookup.
- `svn_wc::status`, the separate status module used by `status_vs_svn` / `status_bench` tests,
  not the app, was not changed.
