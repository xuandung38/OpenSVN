# mark_move, set_depth, upgrade_format (#90, #109)

Branch `feat/mark-move-set-depth`, PR #116. Date: 2026-10-14.

## What landed
- `svn_client::local::mark_move` (+ `svn_wc` `WcDb::op_mark_move`, `db/mark_move.rs`): records a move made
  outside version control like TortoiseSVN "Repair move" (revert the scheduled delete / plain add, rename
  back, `svn move`), writing the same NODES/ACTUAL rows without touching the disk. Shapes: missing or
  deleted source; unversioned, plainly added (reverted first, like TSVN) or copied-from-source destination;
  re-marking a recorded move is a no-op; mixed-revision source → copy + delete without move bookkeeping,
  exactly like `svn move --allow-mixed-revisions`. Refusals (`WcInvalidOperation`): source still on disk,
  kind mismatch, versioned destination, conflicts, replaced source, nested working copy at destination.
- `svn_client::set_depth` (`src/sparse.rs`) = `svn update --set-depth`: crop is a port of svn 1.15
  `crop.c` (`crop_children` + `svn_wc__node_has_local_mods`): a child leaves only when nothing below it is
  modified (deletes aside); otherwise it stays versioned and directories are cropped to `empty`. svn keeps
  modified items **versioned** (not unversioned, as the task note guessed) — verified, matched.
  `exclude` like `svn_wc_exclude` (files too). Then a sticky-depth update (`UpdateParams::sticky_depth`) to
  HEAD widens the tree. Removals go through `file-remove`/`dir-remove` work items; queued removals run even
  if cropping fails or is cancelled. Symlinked targets are refused.
- `svn_wc` `WcDb::sparse_remove` (`db/sparse.rs`): `db_base_remove(keep_as_working=FALSE)` rows. Deviation:
  nothing is queued for not-present/excluded nodes (svn queues a `file-remove` there, which can delete an
  unversioned file of the same name).
- `svn_client::wc::upgrade_format`: same format → no-op, otherwise `WcFormatUnsupported` (format 32 post-MVP).
- RealEngine: `do_mark_move`, `do_set_depth` (session anchored at the folder), `do_upgrade_format` delegate.
  App tests updated; new `set_depth_crops_without_losing_local_changes_and_widens_again`.

## Update machinery changes (needed for correctness against real servers)
- Fake RA (`svn-ra-api/src/fake/update_plan.rs`) now follows svn_repos' `delta_dirs`: honours reported
  depths, requested depth, depth upgrades, excluded entries. It used to send everything, which hid that
  `svn update --depth D` (non-sticky) never recorded the not-present markers of issue #3569 against a real
  server.
- svn-client now sends the request depth like svn (only sticky depths; otherwise `unknown` + narrower report
  depth), reports children like svn's crawler (`report_revisions_and_depths` conditions), restores missing
  items only within the crawl depth, and records #3569 markers by listing the target (svn's
  `fetch_dirents_func`), best effort.

## Tests
- `crates/svn-client/tests/mark_move_vs_svn.rs` (8) and `set_depth_vs_svn.rs` (6): svn 1.15.0 oracle (built
  from the tarball, `OPENSVN_REQUIRE_SVN=1`), NODES/ACTUAL rows, files and `svn status` identical.
- Manual check over real Apache 2.4 + mod_dav_svn (svn 1.14.3 client as oracle; the 1.15 build has no serf):
  crop/grow/immediates/files sequence plus `update --depth empty` and full update matched rows, files and
  status. Not committed (needs a server).
- Oracle harness requires svn ≥ 1.15, so the 1.14 runs skip; 1.14 checked by hand (crop semantics identical).

## Gates (merged origin/main e879ece, includes WP8 #110)
fmt ok; clippy workspace ok; clippy opensvn real-engine (and real-engine,engine-conformance) ok;
`cargo test --workspace --locked` 1315 passed, 0 failed, 21 ignored (Docker/DAV/benchmarks);
`cargo test -p opensvn --features opensvn/real-engine` 453 passed. Frontend untouched (no IPC change).
WP8's `mark_move_and_set_depth` known gap is now a normal scenario in `conformance_tests.rs` and
`tests/real_engine_dav.rs`. `real_engine_dav` (all 16 tests, `--ignored`) passes against a local
Apache 2.4 + mod_dav_svn 1.14.3 over HTTPS with Basic auth (`OPENSVN_COMPAT_LOCAL_*`).

## Found, not fixed (follow-up suggested)
- `build_report` reports anchor-relative paths; the HTTP layer requires the target first as `""`. Updating a
  sub-path with a session at the working copy root fails over DAV (fake accepts it). `set_depth` avoids it by
  anchoring at the folder; `do_update` with paths still hits it.
- svn 1.15 itself leaves the work queue and locks behind when `--set-depth exclude` falls back to cropping;
  OpenSVN runs the queue (tests compare after `svn cleanup` on the svn side).
