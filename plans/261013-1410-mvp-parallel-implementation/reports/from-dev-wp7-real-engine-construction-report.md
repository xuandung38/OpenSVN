# WP7: real engine construction and engine selection

Branch `feat/wp7-real-engine-construction` from `origin/main` cbe88ab. Plan:
`real-engine-wiring-plan.md`, decision C, WP7 and Rollout.

## What changed

- **`app_state.rs`**
  - `real_engine_source()` (feature `real-engine`) returns `EngineSource::Build`.
  - `build_real_engine(&EngineServices)` builds `RealEngine` from:
    - `HttpConnector::from_services`
    - `SqliteLockOwnership::with_os_probe(db)`
    - `lock_ownership::current_boot_id()`
    - `ClientRecovery`
    - `state_dir = <data_dir>/state`, created through `prepare_private_dir` (0700 on Unix,
      because commit journals hold log messages)
  - `wc.readSubversionConfig`:
    - At start, `set_subversion_config_dir(default_subversion_config_dir())` is called when the
      setting is on, or `None` when it is off.
    - A task on `tauri::async_runtime` follows the settings channel and calls it again when the
      value changes. The task holds only a `Weak` to the engine and ends when the engine or the
      settings service is dropped.
- **`lib.rs`: engine selection, `choose_engine(env, release)`**
  - Feature `real-engine` compiled in: `RealEngine` is the default. `OPENSVN_ENGINE=fake`
    (case-insensitive, trimmed) selects `FakeEngine`.
  - Feature not compiled in: `FakeEngine`, as before. `OPENSVN_ENGINE=real` is ignored with a
    warning.
  - Any other value is ignored with a warning, and the default engine runs.
  - A release build (`!cfg!(debug_assertions)`) running `FakeEngine` logs `FAKE ENGINE IN USE: ...`
    at warn level. With `real-engine` compiled in, a release build only runs the fake engine when
    `OPENSVN_ENGINE=fake` is set. Without the feature, release builds still run the fake engine,
    because there is nothing else to run; they also log the warning. These builds were not made
    to refuse to start, so today's release builds keep working until the rollout.
  - Data directory: the real engine uses the app data directory itself. The fake engine keeps
    `<data>/fake-engine`.
  - Warnings are logged after `logging::init`, followed by `engine selected engine=...`.
  - `finish_startup` registers the demo working copy only for the fake engine.
- **Temporary `#[allow(dead_code)]` markers from WP2 removed**
  - `HttpConnector::from_services`: the marker is gone because the method is now used.
  - `AuthBroker::trust_store` / `invalidations`: these are now
    `#[cfg(any(test, feature = "real-engine"))]`.
  - `EngineServices`: `#[cfg_attr(not(feature = "real-engine"), allow(dead_code))]`, because only
    the real builder reads every field.
  - `EngineSource::Build`: `cfg_attr(not(any(test, feature)))`.
- **Cargo `[features]`**: comments updated only. `real-engine` is **not** a default feature.
- **`engine/real/mod.rs`**: adds a `#[cfg(test)] subversion_config_dir()` getter. Nothing else
  changed there. `real/commit.rs` and connector/auth logic were not touched; only the
  attribute lines above changed.
- **Docs**: `docs/system-design/02` data directory row: real engine directory, `state/`, and
  `OPENSVN_ENGINE`.

## Tests

- `app_tests/engine_startup.rs`:
  - `AppState` builds over `Ready(FakeEngine)`.
  - A `Build` closure receives the services (data dir and settings).
  - A failing builder fails the start.
  - With `real-engine`:
    - `AppState` builds over `real_engine_source()`, `state/` exists, no working copy is
      registered, and the demo folder is not accepted as a working copy.
    - The config directory follows `wc.readSubversionConfig`: on → off → on.
    - The setting off at start gives `None`.
- `lib.rs` tests:
  - `fake` / `FAKE` / ` fake ` select the fake engine.
  - A release build with the fake engine gives the loud warning.
  - An unknown value is ignored with a warning.
  - Data directory per engine kind.
  - The real engine is the default when compiled in (`None`, `""`, `real`, `Real`), and its
    source is `Build`.
  - Without the feature: fake by default, and `real` ignored with a warning.
- `app_tests/fixture.rs`: `state_with_engine(dir, EngineSource)` helper. The existing fixture
  still uses `Ready(FakeEngine)`, and all existing app tests pass.

## Gates

- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: pass
- `cargo clippy -p opensvn --features real-engine --all-targets --locked -- -D warnings`: pass
- `cargo test --workspace --locked --no-fail-fast`: 1276 passed, 0 failed, 21 ignored
- `cargo test -p opensvn --features opensvn/real-engine --locked`: 405 passed, 0 failed
- `gen:ipc` (`gen_ipc` binary): no drift in `bindings.ts`; `committed_bindings_are_up_to_date`
  passes
- `cargo build -p opensvn --features real-engine --locked`: links
- Smoke test: the debug binary ran under `xvfb-run` for 15 s with a scratch HOME/XDG_DATA_HOME.
  - Unset env logged `engine selected engine=Real` and wrote `app.sqlite` and `state/` in the
    data directory.
  - `OPENSVN_ENGINE=fake` logged `engine=Fake` and used `fake-engine/`.
  - No panic in either run.
- Not run:
  - `pnpm tauri build --debug --features real-engine`: no node_modules install, so the
    `cargo build` above stands in for it.
  - The release-mode build.
  - Any real server.
  - FE lint/typecheck/test: no FE files changed.

## Remaining

- Rollout:
  - Make `real-engine` a default feature only after WP8(b) (conformance against Apache) and a
    manual VisualSVN smoke test on three OSes.
  - Turn on `negotiate-gssapi` in the macOS/Linux release workflow (this repo has no release
    workflow yet).
- E2E / FE dev scripts should set `OPENSVN_ENGINE=fake` once release builds include the real
  engine. Today no script needs it, because the default build has no real engine.
