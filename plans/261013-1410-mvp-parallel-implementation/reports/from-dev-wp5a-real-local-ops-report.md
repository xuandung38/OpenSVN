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
