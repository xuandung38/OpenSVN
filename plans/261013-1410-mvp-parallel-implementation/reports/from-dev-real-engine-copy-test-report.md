# Report: real-engine copy test after fake RA changes

Branch: `fix/real-engine-copy-test` (from origin/main 802822f)

## Symptom
`cargo test -p opensvn --features opensvn/real-engine` failed on main:
`engine::real::tests::edit_tests::copy_reports_what_the_repository_cannot_do` got `PathNotFound`
where it expected `ServerUnsupported`.

## Root cause
Both halves contributed, but the fake did not regress:

1. **Test premise obsolete.** Before #76 the fake commit editor refused every `copy_from` with
   `ServerUnsupported`, which the test relied on. #76 added real copy support to the fake.
2. **Latent RealEngine bug exposed.** `RealEngine` (remote_edit.rs `plan_copy`) passed the copy
   source as a repository fspath (`/trunk`). The commit-editor contract, as used by svn-client
   (`commit/drive.rs::copy_arg`) and as svn's RA layer (ra_serf/ra_svn) takes it, is a full
   **URL**. The new fake resolves sources as URLs, could not find `/trunk`, and correctly
   answered `PathNotFound`. Against a real server, RealEngine's server-side copy/branch/rename
   would have failed the same way.

## Fix
- `apps/opensvn/src-tauri/src/engine/real/remote_edit.rs`: copy source is now
  `session.repos_root().join(from)` as a URL (removed `fspath`).
- `crates/svn-ra-api/src/commit.rs`: `add_directory`/`add_file` docs state the copy source is
  `(URL, revision)`.
- `crates/svn-ra-api/src/fake`: opt-in `FakeRaSession::without_copies()`; its commit editors
  refuse copies with `ServerUnsupported` (default behaviour unchanged).
- Tests (`edit_tests.rs`): `copy_reports_what_the_repository_cannot_do` now runs against a
  `without_copies()` repository and keeps its assertions (`ServerUnsupported`, no open txn,
  "already exists"); new `copy_creates_the_branch_from_its_source_url` proves a real copy works
  end to end (new revision, copied file readable). Drive unit test expectations use URLs.

## Gates (local, CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0)
- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: pass
- `cargo clippy -p opensvn --features real-engine --all-targets --locked -- -D warnings`: pass
- `cargo test --workspace --locked --no-fail-fast`: 1098 passed, 0 failed (111 binaries)
- `cargo test -p opensvn --features opensvn/real-engine --locked`: 293 passed, 0 failed
- FE gates: not run (no FE files touched)

## Remaining
None for this fix. Copy from a working copy (`from_wc`) in RealEngine is still `not_wired`.
