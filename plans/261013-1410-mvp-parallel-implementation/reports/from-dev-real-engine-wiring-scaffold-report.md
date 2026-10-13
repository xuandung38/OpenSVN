# WP-S: real engine wiring scaffold

Branch `feat/p15-real-engine-wiring-scaffold` (from `origin/main` 802822f). Plan:
`plans/261013-1410-mvp-parallel-implementation/real-engine-wiring-plan.md`.

## Done (no behavior change)
- `svn-client` is an optional app dependency enabled by `real-engine`.
- Every unwired `EngineFacade` method of `RealEngine` delegates to a `do_*` method in its own file,
  each still returning the same `not wired` error:
  - `real/wc_status.rs`: status, diff_wc, status_remote (WP3)
  - `real/sync.rs`: checkout, update, switch, set_depth (WP4)
  - `real/local.rs`: revert, add, delete, move, resolve, cleanup, changelist_set, ignore_add,
    props_set, mark_move, upgrade_format (WP5a)
  - `real/commit.rs`: commit, copy_location from a working copy (WP5b)
- `ConnectOptions { anchor: Anchor::Root | Anchor::Url, interactive }` (with `root(ctx)` /
  `at_url(ctx)` helpers) and `SessionConnector::connect_with` whose default ignores the options and
  calls `connect`; existing connectors unchanged.
- `RealEngine::client_ctx(&OpContext) -> svn_client::ClientCtx` (lock owner = this process via
  `lock_owner()`, default config, no progress; WP4 maps progress).
- CI: Linux step runs clippy and tests for `-p opensvn --features real-engine`.
- Tests: `real/tests/scaffold_tests.rs` (connect_with default, client_ctx owner).

Untouched: lib.rs, app_state.rs, menu, FE (PR #81).

## Gates (local)
- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets -- -D warnings`: pass
- `cargo clippy -p opensvn --features real-engine --all-targets -- -D warnings`: pass
- `cargo test -p opensvn`: 227 passed, 0 failed
- `cargo test -p opensvn --features opensvn/real-engine`: 293 passed, 1 failed:
  `engine::real::tests::edit_tests::copy_reports_what_the_repository_cannot_do`.
  **Pre-existing on main** (same failure with this branch's changes stashed). Since #76 the fake
  commit editor supports copies but resolves copy sources only as URLs; `remote_edit.rs` passes an
  fspath (`/trunk`), so the fake answers PathNotFound instead of the ServerUnsupported the test
  expects. The HTTP editor (#75) accepts both forms. Fix (follow-up, not in this scaffold):
  pass the source URL from `plan_copy` and turn the test into a real copy test. Until then the
  new CI step is red on this test.
