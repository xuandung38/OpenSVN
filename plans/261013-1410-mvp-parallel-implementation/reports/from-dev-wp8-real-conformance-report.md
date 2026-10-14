# WP8: real engine conformance (fake repository and Apache + mod_dav_svn)

Branch `feat/wp8-real-conformance`, based on origin/main cbe88ab plus `origin/feat/wp5b-real-commit`
(PR #105, not merged yet; commit scenarios need it). When #105 lands, merge main instead.

## What changed

- **(a) `engine/real/tests/conformance_tests.rs`**: every scenario of `conformance::run_all` runs as
  its own test against `RealEngine`. Sessions come from `AnchoredConnector` from `support.rs`
  (`FakeRaSession::at` for `Anchor::Url`, the same connector `commit_tests.rs` uses). The only
  gap is `mark_move_and_set_depth`. It is pinned as a `should_panic` test on the exact
  `mark_move` refusal (#90, #109), so closing the gap makes that test fail.
- **(b) `apps/opensvn/src-tauri/tests/real_engine_dav.rs`** (`#[ignore]`, features
  `real-engine,engine-conformance`) runs `RealEngine` over the real `HttpConnector` (test CA
  in `extra_root_certs`, no proxy, Basic `tester`/`tester-pass`).
  - Server selection: `OPENSVN_COMPAT_LOCAL_URL`/`_REPOS` with CA from
    `OPENSVN_COMPAT_LOCAL_CA` or `OPENSVN_COMPAT_CA`. Otherwise the compat containers on
    :8443/:8444, as in `compat_write.rs`.
  - Each test loads the standard tree into a fresh scratch repository with `svnadmin load`
    (a generated dump stream, no svn client needed) and deletes it afterwards.
  - The authz case (deny `/`, allow `/trunk`) needs `OPENSVN_COMPAT_LOCAL_AUTHZ`. The compat
    containers have no `mod_authz_svn`, so there the case prints `SKIPPED` and passes.
  - `HttpConnector::with_config_tweak` is now also public under `engine-conformance`.
  - `.github/workflows/compat.yml` has a new step that runs this test against both containers.
    It also installs the Tauri libraries, builds the frontend, and adds the engine paths to
    the workflow triggers.
- **(c) New scenarios** in `engine/conformance.rs`: `locked_unmodified_file_in_status`,
  `commit_with_auto_add_remove` and `update_ends_in_conflict`.
  - `Harness` gained `remove_file`.
  - `working_copy_metadata_operations` was split: `mark_move` and `set_depth` moved to the new
    `mark_move_and_set_depth`.
  - FakeEngine passes all 15 scenarios without changes.

## Discrepancies found and fixed (each with a test)

1. **FakeRaSession commit editor** reported an added file twice in the changed paths (`A`, then
   `M` when its text arrived). Delete followed by add was reported as `D` + `A` instead of `R`.
   It now keeps one entry per path, like svn.
   Tests: `crates/svn-ra-api/tests/fake_commit_and_reads.rs` (`commit_creates_a_new_revision`
   asserts the exact list; new `delete_and_add_of_the_same_path_is_one_replace`).
2. **svn-ra, authz denying the root**:
   - `youngest_rev` (and every read without a revision, through `peg`) sent `OPTIONS` to the
     session URL. The engine's root-anchored session got 403 there. It now retries once at the
     URL the session was opened at (`RaWire::opened_url`, kept by `reparent`).
   - The log `REPORT` went to the session root. Like ra_serf, it now goes to the deepest common
     ancestor of the requested paths.
   - Tests: `crates/svn-ra/tests/session_plumbing.rs`
     `reparented_root_session_works_when_the_root_is_forbidden`, unit test in `ops/log.rs`,
     and the DAV authz case.

## Scenario results

| Scenario | FakeEngine | RealEngine + fake repo | RealEngine + Apache 2.4.58 / mod_dav_svn 1.14.3 (HTTPS, Basic) |
| --- | --- | --- | --- |
| repository_reads | pass | pass | pass |
| commit_and_update_cycle | pass | pass | pass |
| schedule_and_revert | pass | pass | pass |
| conflict_blocks_commit_until_resolved | pass | pass | pass |
| lock_and_unlock | pass | pass | pass |
| remote_edit_and_copy | pass | pass | pass |
| cancellation_and_errors | pass | pass | pass |
| working_copy_metadata_operations | pass | pass | pass |
| mark_move_and_set_depth | pass | known gap #90/#109 (pinned) | known gap #90/#109 (pinned) |
| switch_to_a_branch | pass | pass | pass |
| repository_content_tools | pass | pass | pass |
| open_recovers_dead_locks_and_reports_foreign_ones | pass | pass | pass |
| locked_unmodified_file_in_status (new) | pass | pass | pass |
| commit_with_auto_add_remove (new) | pass | pass | pass |
| update_ends_in_conflict (new) | pass | pass | pass |
| authz deny `/`, allow `/trunk` (DAV only) | n/a | n/a | pass |

DAV run: `cargo test -p opensvn --features real-engine,engine-conformance --locked --test
real_engine_dav -- --ignored` reported **16 passed, 0 failed**: 14 scenarios, the pinned gap and
the authz case. Apache ran locally (config in the scratchpad `davwp8/`) and was stopped
afterwards. The compat containers (1.14.5/1.15.0) were not run locally (no Docker); CI runs
them through the new compat.yml step.

## Gates

svn 1.15.0 oracle env, OPENSVN_REQUIRE_SVN=1. All exit 0.

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo clippy -p opensvn --features real-engine,engine-conformance --all-targets --locked -- -D warnings`
- `cargo test --workspace --locked`: 1271 passed, 0 failed, 21 ignored
- `cargo test -p opensvn --features opensvn/real-engine --locked`: 420 passed, 0 failed
- DAV run above: 16 passed

## Remaining

- #90 (mark_move, upgrade_format 31<->32) and #109 (set_depth): new issue, opened for this.
- Authz case on the compat containers: they need `mod_authz_svn` plus an authz file (docker/
  belongs to P06). Until then it skips there.
- `conformance_recovery_tests.rs` still wraps checkout through `svn_client` directly, although
  the engine's own checkout is wired now. `conformance_tests.rs` covers the same scenario
  through the engine, so the wrapper could be removed.
