# Hotfix: real-engine build and biased select on main

Branch `fix/main-real-engine-build`, from `origin/main` at `437bbdc` (after #62, #69, #73, #80, #82).

## Failures found on origin/main

1. `cargo clippy -p opensvn --features real-engine` did not compile:
   - `RealEngine` was missing `EngineFacade::ensure_authenticated` (added by #69).
   - `RealEngine::copy_location` still had the old 3-parameter signature. The new
     `from_wc: Option<&WcRef>` parameter came from #73. The real-engine tests in `edit_tests.rs`
     also lacked `CopyLocationRequest::from_wc`.
2. `cargo test --workspace` (default features) failed:
   `engine::fake::tick_tests::every_select_in_the_app_is_biased`. It flagged the
   `tokio::select!` in `engine/real/tests/remote_tests.rs::drop_while_the_directory_is_locked`
   (#80). The check scans the source text, so it fails even when real-engine is not built.

Nothing else failed.

## Fixes

- `RealEngine::ensure_authenticated` follows the semantics of `FakeEngine`:
  - Non-interactive contexts return `Ok` and never prompt.
  - Otherwise it reads the working copy's repository root from `wc.db` (`wc_read::wc_info`, no
    lock). It then connects through the `SessionConnector` and calls
    `RaSession::ensure_authenticated`, using the existing `locate_repo_path` helper.
- `RealEngine::copy_location` now takes `from_wc`. A copy from a working copy needs the
  svn-client working copy layer, so it returns the usual `not wired` error. URL-to-URL copies
  are unchanged.
- `biased;` was added to the select. `op` stays the first branch, so the order only means that
  "op finished" is checked before "tick elapsed". The panic-if-it-completes semantics are unchanged.
- New test `ensure_authenticated_signs_in_to_the_working_copy_repository_only_when_interactive`
  in `engine/real/tests/recovery_tests.rs`. A background context makes no auth call. An
  interactive context makes exactly one.

## Gates (all on this branch)

| Gate | Result |
| --- | --- |
| `cargo fmt --all --check` | pass |
| `cargo clippy --workspace --all-targets -- -D warnings` | pass |
| `cargo clippy -p opensvn --features real-engine --all-targets -- -D warnings` | pass |
| `cargo test --workspace --locked --no-fail-fast` | pass: 1039 passed, 0 failed, 10 ignored |
| `cargo test -p opensvn --features opensvn/real-engine` | pass: 292 passed |
| `pnpm install --frozen-lockfile` | pass (node 22 here; engines asks for >=24, warning only) |
| `pnpm lint` (root and app) | pass |
| `pnpm typecheck` | pass |
| `pnpm lint:safety` | pass |
| `pnpm test` | pass: 183 tests in 25 files |
| `pnpm build` (includes `check-dist`) | pass |
| `pnpm gen:ipc` drift | none |

PR #81 contains the same `biased;` line, so the two changes are identical and merge without
conflict.
