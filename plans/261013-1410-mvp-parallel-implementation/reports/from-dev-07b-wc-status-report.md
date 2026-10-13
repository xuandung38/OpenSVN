# dev report: 07B M9 working-copy status (PR #71)

Branch `feat/07b-wc-status`. origin/main merged in with a merge commit. The only conflict was
`crates/svn-wc/Cargo.toml`, where both sides added dependencies; I kept all of them
(`ignore`, `svn-diff`, `unicode-normalization`).

## Wider oracle matrix (`crates/svn-wc/tests/status_vs_svn.rs`)
Our status is compared with real `svn status --xml --no-ignore --ignore-externals`, both in the
default mode and with `-v` (`get_all`). Per entry it compares `item`, `props`, `copied`,
`switched`, `file-external`, `tree-conflicted`, `moved-to`, `revision` and the commit revision.
Every scenario first asserts that svn itself reports the state the scenario claims to build, so
a test cannot pass vacuously.

| Scenario | States covered |
|---|---|
| common | M, props M, D, missing, A, A+ copy, move (moved-to), changelist, ?, ignored, unversioned dir |
| replaced | R (delete + add), R+ (replaced by a copy), replaced directory |
| missing/obstructed | missing directory, file to dir, dir to file |
| conflicts | text, property, tree (edit vs incoming delete, delete vs incoming edit), markers removed by hand |
| externals ignored | dir external (`external`), file external (`file-external`), modified file external |
| switched | switched dir, switched file, modified file in a switched dir |
| depth limited | `--depth immediates`, `--set-depth exclude`, `--set-depth empty` with an unversioned leftover, `--depth files` |

Real differences the new matrix found, all fixed in `src/status/`:
1. Copies and moves were reported as `switched`. NODES `repos_path` of a copy is the copy source;
   svn never reports an added node as switched.
2. File externals were reported as `switched`, and a default (non `-v`) status dropped them.
   svn always lists them, with `file-external="true"`.
3. Directory externals were reported as `unversioned`. They now come from the EXTERNALS table as
   the new `ItemStatus::External`, and are never classed as ignored.
4. Deleted nodes reported no last-changed revision, author or date. They now take these from the
   layer they delete, as svn's `read_info` does.
5. Replaced nodes reported no BASE revision. New `StatusEntry::xml_revision()` follows
   `print_status_xml`: no revision for copies, `-1` for plain additions, the BASE revision for
   replacements.

The comparison runs against the local svn (oracle crate, 1.14.3 on this host). It does not go
through the Docker compat harness: the harness has no engine driver yet, and Docker has no daemon
on this host.

## 100k benchmark (`crates/svn-wc/tests/status_bench.rs`, `#[ignore]`)
The test writes a synthetic working copy directly into wc.db: 100 dirs of 1 000 files, one shared
pristine, and no recorded stat. Run it with
`cargo test --release -p svn-wc --test status_bench -- --ignored --nocapture`.

Release build, Linux x86_64, 4 vCPU, shared host with load average about 15-20 (other agents'
builds):

| Phase | Time |
|---|---|
| build fixture (100 000 files) | 15.3 s |
| cold status: content compare of every file plus stat cache repair (one transaction) | 4.60 s |
| warm status (size+mtime fast path), 3 runs | 1.16 s / 0.85 s / 0.94 s |
| warm status with one modified file | 0.93 s (reports exactly 1 `M`) |
| teardown | 2.05 s |

A first run under heavier load gave cold 6.5 s and warm 1.47-1.61 s. The warm target of under
3 s is met, and the test asserts it in release builds. The cold run is above 3 s; it only happens
when no stat cache exists (fresh checkout by another client, or touched mtimes). The test also
checks that the cold run repairs all 100 000 cache entries.

## Gates (local)
- `cargo fmt --all --check`: pass.
- `cargo clippy -p svn-wc --all-targets -D warnings`, with and without `--all-features`: pass.
  Scoped to svn-wc: no other crate uses `svn_wc::status` (checked with grep). A workspace-wide
  clippy was not run because of host load.
- `OPENSVN_REQUIRE_SVN=1 cargo test -p svn-wc`: 208 passed, 0 failed, 1 ignored (the benchmark).
  This includes `status_vs_svn` 7/7 against svn 1.14.3.
- `cargo test -p svn-wc --features failpoints`: 209 passed, 0 failed.
- Benchmark in release: pass.

## Remaining
- svn 1.15 was not available here. Status logic is identical in 1.14.5 and 1.15.0, but the
  matrix ran on 1.14.3 only.
- Not covered yet:
  - descending into externals (svn's per-external targets);
  - `wc-locked` and lock tokens;
  - `svn:special`, eol and keyword files in the matrix (`file_modified` handles them, but they
    have no oracle case);
  - interrupted update (`incomplete`).
- The cold-path speed (full content compare) could be improved later with more workers or
  mmap; it is not part of the target.
