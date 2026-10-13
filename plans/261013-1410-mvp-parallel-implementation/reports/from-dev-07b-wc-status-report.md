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

## Review round 1 (PR #71 review 5504289025)

### Fixed
- **B1** `load_nodes` decoded the property skel with `decode_props` and checks for the
  `svn:special` key; `had_props` is "decoded list not empty" (svn's
  `SQLITE_PROPERTIES_AVAILABLE`). Regression oracle case
  `status_matches_svn_for_svn_special_text_in_other_properties`: `svn:special` inside a property
  value and inside a longer property name, checked both on the fast path and with the content
  compare. It fails on the old code (both files `obstructed`) and passes now.
- Reviewer's extra oracle case added as
  `status_matches_svn_for_translation_symlinks_and_subdir_target`: `svn:keywords` Id and Rev
  (Rev with a body edit), `svn:eol-style` CRLF and native, CRLF file rewritten with LF (not
  modified), same-size content change, touched files, symlink retargeted and replaced by a regular
  file, and status on the targets `d` and `d/f`.
- **Follow-up 1** format 32 without pristines: `DbPristineSource::open_text` returns
  `NotHydrated` for a `hydrated = 0` row, and `compare_with_pristine` then compares the SHA-1 of
  the repository-normal working text with the recorded checksum, the same as svn 1.15.0's
  `compare_and_verify` (checked against the 1.15.0 source). Oracle case
  `status_matches_svn_without_stored_pristines` (svn 1.15, `--store-pristine=no`, touched, eol
  repaired, keyword, same-size and different-size edits) plus the svn-independent unit test
  `dehydrated_pristine_is_compared_by_checksum`. The oracle case fails on the old code.
- **Follow-up 4** `scan.rs` keeps the I/O classification of walk errors (`SvnError::from_io`
  codes); a walk error without an I/O cause (symlink loop) is `Internal`. Unit test added.
- **Follow-up 6** the empty `src/status/tests.rs` is removed.

### Not done (still follow-ups)
- 2 (`wc-locked` / lock token columns), 3 (output order assertion), 5 (lock ownership check for
  `update_cache`), 7 (`in_scope` allocations).

### Gates (round 1)
- `cargo fmt --all --check`: pass.
- `cargo clippy -p svn-wc --all-targets -- -D warnings`: pass.
- `cargo clippy --workspace --all-targets -- -D warnings`: pass (after the merge of origin/main).
- `OPENSVN_REQUIRE_SVN=1 cargo test -p svn-wc` with svn 1.14.3: 212 passed, 0 failed, 1 ignored
  (the benchmark); `status_vs_svn` 10/10 (the format-32 case skips on 1.14).
- Same with `OPENSVN_SVN_BIN` = svn 1.15.0 (nix store): 212 passed, 0 failed, 1 ignored;
  `status_vs_svn` 10/10 with the format-32 case run.
- The release benchmark was not re-run.
