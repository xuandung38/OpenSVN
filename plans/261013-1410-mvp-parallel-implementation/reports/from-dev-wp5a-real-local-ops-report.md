# WP5a: real engine local working copy operations

Branch `feat/wp5a-real-local-ops` (from origin/main 8c04160).

## What changed
- `engine/real/local.rs`: revert, add_paths, delete_paths, move_path, resolve, cleanup, changelist_set,
  ignore_add, props_set wired to `svn_client::local`. Each call opens the registered root with
  `svn_client::wc::open_containing` (lock owner from `client_ctx`) inside `wc_read::blocking`; svn-client
  takes the WC write lock (section E, no app lock).
  - revert: recursive; `remove_unversioned` collects unversioned (not ignored) items below the targets
    via local status *before* the revert (reverted additions stay on disk, like svn/FakeEngine), then
    deletes them, all under one `with_wc_lock`.
  - add: depth infinity, add_parents, `AddConfig` (global-ignores + auto-props) from the configured svn
    config dir plus the WC's local ignore list.
  - delete: keep_local passthrough, force false, effective global ignores.
  - resolve: Mine->MineFull, Theirs->TheirsFull, Working, Base, `ResolveScope::ALL`, non-recursive.
  - cleanup: `local::cleanup`; `needs_user_action` lock -> `WcBusy`; then removes unversioned and/or
    ignored items found by status under the lock.
  - changelist_set: `Depth::Empty`, FakeEngine name check. props_set: `propset`.
  - ignore_add: `SvnIgnore` -> `local::ignore_add`; `Local` -> per-WC list in
    `.svn/opensvn-local-ignores` (FakeEngine keeps a per-WC list too); add/delete/cleanup treat it as
    global ignores. Pattern check mirrors FakeEngine (non-empty, no `/`).
  - mark_move -> `wc-invalid-operation`, upgrade_format (to a different format) ->
    `wc-format-unsupported`, both with a user message linking issue #90. upgrade to the current format is
    a no-op (conformance expects that).
- `engine/real/local_util.rs` (shared, for WP5b auto_add_remove): `open_wc_root` (refuses a non-root),
  `target`/`targets` (RelPath + `safe_join`: refuses `..`, absolute/drive paths, `.svn`, symlinked
  intermediate dirs -> `UnsafePath`), `local_env` (effective ignores + AddConfig), `unversioned_items`,
  `remove_items` (never follows symlinks), local ignore list read/write, `SvnConfigSource`.
- `engine/real/mod.rs`: `mod local_util`, `svn_config` field, `RealEngine::set_subversion_config_dir`,
  re-exports `SvnConfigSource`, `default_subversion_config_dir`, `read_local_ignores` (WP3 status and WP7
  settings wiring should use these: status must append `read_local_ignores(root)` to the global ignores;
  WP7 should call `set_subversion_config_dir(default_subversion_config_dir())` while
  `wc.readSubversionConfig` is on; default is svn's built-in defaults).
- Dev-dependency `opensvn-svn-oracle` on the app.

## Tests (`engine/real/tests/local_tests.rs`, real-engine feature)
Local parts of `schedule_and_revert`, `working_copy_metadata_operations`,
`conflict_blocks_commit_until_resolved` (all four resolve choices) on WCs checked out by svn-client from
the in-memory repo; revert remove_unversioned, delete refusal/keep_local, add parents + ignores, cleanup
variants, needs-user lock -> WcBusy, configured auto-props/global-ignores, cancellation, nested root
refusal, path safety (8 calls x 6 bad paths incl. symlink escape, plus ignore dir). Oracle module repeats
schedule/move/revert, metadata and conflict-theirs on WCs made and checked by svn 1.15 (`svn status`).

## Not done / follow-ups
- The full conformance scenarios still need engine checkout/status (WP3/WP4) before `run_all` can run
  on RealEngine (WP8a).
- Issue #90: svn-client support for mark_move and format upgrade.
- No progress events for local ops (short calls).

## Review round 1 fix (blocking: externals / nested WCs deleted by remove_unversioned)
- Root cause fixed in `svn-client` status: new `status/externals.rs` parses `svn:externals` (svn 1.5+
  `[-r N] URL[@PEG] PATH` and pre-1.5 `PATH [-r N] URL`, quotes and backslash escapes, comments) and
  combines the definitions (pristine + local, on the target's ancestors and every walked directory) with
  the wc.db `EXTERNALS` rows. The walker reports a directory external, and any unversioned directory
  leading to one (svn's `is_external_path`), as the new `NodeStatus::External` (`X`), not descended into
  (= `svn status --ignore-externals`). File externals: new `WcStatus::file_external`, no longer reported
  as switched, always listed (svn column 5 `X`). `opensvn-ipc` maps `External` -> IPC `External`
  (WP3/PR #93 gets this for free through `file_status_from_wc`).
- A separate checkout nested inside the WC stays `?` in status, exactly as svn 1.15 shows it.
  `svn_client::status::is_wc_root_dir` lets callers recognise it.
- `local_util::unversioned_items` never returns externals (or their leading dirs) nor nested WC roots,
  also when they are named directly as revert targets. `remove_items` removes an unversioned directory
  except any WC nested inside it, tries every item and reports all failures in one error (first failure
  as cause); cancellation still stops at once. Windows dir symlinks/junctions fall back to `remove_dir`.
- Deliberate divergence from svn 1.15: `svn cleanup --remove-unversioned` deletes a nested checkout; the
  app keeps it (documented on `unversioned_items`). The property-based detection can also mark a defined
  but never-fetched external directory as `X` where svn (no EXTERNALS row) shows `?` - errs on the side
  of not deleting.
- Tests: svn-client oracle `status_reports_externals_and_nested_checkouts_like_svn` (dir external,
  old-format external with a space, external two unversioned dirs deep, modified and unmodified file
  externals, nested checkout; compared with `svn status --ignore-externals` on columns 1,2,5,7), parser
  unit tests; engine: status/cleanup/revert with property-defined externals + nested checkouts (always
  run), oracle cleanup(remove_unversioned+remove_ignored) and revert(remove_unversioned) on svn-fetched
  externals with local edits, and remove_items continuing past a failure.
- Still open (non-blocking): `set_subversion_config_dir` wiring is WP7; fake engine `Mine` semantics
  differ from real (real = svn mine-full).

## Review round 2 fix (blocking: a request path inside an external / nested WC was deleted)
- Merged origin/main up to a3e9f2e (WP3 #93, WP4 #95). The one conflict was the module doc in
  `engine/real/mod.rs`; it now lists status (WP3), sync (WP4) and local ops (this PR), and says only
  commit is still unwired.
- New `local_util::ensure_own_path`. It applies to a request path that this WC's wc.db does not know.
  It checks every prefix, from the first component down to the path itself, and refuses the path
  (`WcInvalidOperation`) if any prefix is either of these:
  - a WC root: a directory with its own `.svn`, meaning a fetched external or a nested checkout;
  - an external: an `EXTERNALS` row, or an `svn:externals` definition even when the external is not
    fetched yet.

  The error reads "'ext/main.rs' belongs to an external working copy ('ext'); open that working copy
  to revert or change it". Where it runs:
  - `target()` calls it, so revert, add, delete, move (both ends), resolve, props, changelist and
    ignore all refuse such a path before anything changes. Revert without `remove_unversioned` no
    longer skips the path silently.
  - `unversioned_items` calls it as well, as a second line of defence.
  - Naming an external or nested root directly (`ext`, `nested`) is now an error too; before, it was
    a silent no-op. A directory that only leads to an external (`deep`) is still this WC's, so it is
    accepted and kept.
- `svn_client::status::ExternalPaths` changes:
  - new `is_external` and `within` methods;
  - case-insensitive matching on macOS, iOS and Windows (`CASE_INSENSITIVE_FS`, `ExternalPaths::new(fold)`).
    On a case-sensitive volume of those platforms this only protects more paths, never fewer.
  - A doc comment now says that when the parser skips a definition line it cannot read, the `.svn`
    check (`is_wc_root_dir`) is the safety net.
- WP3's `TODO(#94)` is done: `do_status` now uses as global ignores the configured ones (or svn's
  defaults) plus `read_local_ignores`. This is the same list the local ops use.
- New tests:
  - Oracle regression tests (svn 1.15) for the three probe cases: `ext/main.rs`, `nested/README.md`,
    and `u/inner/src` (a checkout hidden in an unversioned dir). Each must fail with the error and leave
    the inner WC intact, which is checked with svn.
  - Always-run engine tests:
    - paths inside externals and nested WCs are refused for revert, add, delete and move;
    - a defined but unfetched external that holds user files (no `.svn`) shows as `X`, its contents
      are refused, and cleanup and revert keep it;
    - the engine status applies the local ignore list.
  - Unit tests for `within` and for case folding.
- Non-vacuity, checked locally:
  - With `ensure_own_path` disabled, 7 tests fail: the 3 oracle cases, the 3 new engine tests and the
    updated named-target test.
  - With `ExternalPaths::is_external` and `covers` forced to false, the unfetched-external test fails.
