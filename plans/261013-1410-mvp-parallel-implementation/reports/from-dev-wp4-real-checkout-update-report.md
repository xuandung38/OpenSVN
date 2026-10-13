# WP4 real checkout / update / switch — dev report

Branch `feat/wp4-real-checkout-update` (from origin/main 793c6f6, includes WP0 #91).

## What changed

- `engine/real/sync.rs`:
  - `checkout`:
    - A format other than 31 fails with `WcFormatUnsupported`.
    - A non-empty `sparse` plan fails with `WcInvalidOperation` and a "sparse checkouts are not supported yet" message. It used to fail with `Internal`. Nothing is created on disk.
    - The session comes from `connect_with(url, ConnectOptions::at_url(ctx))`. The URL is checked to be the one requested; the case-insensitive comparison allows a server that spells the root differently. A connector that returns a root-anchored session is refused instead of checking out the repository root.
    - Then `svn_client::checkout(.., depth, false, &ctx.cancel)`, then `wc_info`.
  - `update`:
    - The session is anchored at the working copy URL.
    - HEAD is resolved once with `youngest_rev` (or `req.rev` is used), then `svn_client::update` runs for each path at that revision. `req.depth: None` means `Depth::Unknown`, i.e. the recorded depths.
    - Returns that `Rev`.
    - Paths are validated with `RelPath::new`.
  - `switch`:
    - The session is anchored at the working copy URL.
    - The target must be inside the session's repository root, compared segment by segment. Otherwise it fails with `WcInvalidOperation`: "not in this working copy's repository".
    - HEAD is resolved once, then `svn_client::switch`.
  - `set_depth`: fails with `WcInvalidOperation` and a "changing the depth ... is not supported yet" message.
- `engine/real/progress.rs`: `client_progress(&ProgressReporter, phase) -> svn_client::ProgressFn`. Each `UpdateEvent` becomes a `ProgressUpdate { phase: "checkout" | "update", current: n (1-based, strictly increasing), current_path: <wc-relative path> }`. One callback is used per operation, so the count keeps rising across the paths of a multi-path update.
- `mod.rs`: one line, `mod progress;`. `tests/mod.rs`: one line, `mod sync_tests;`.

## Tests (`engine/real/tests/sync_tests.rs`, `progress.rs` unit tests)

The test connector (`AnchoringConnector`) serves `FakeRaSession::at(url)` for `Anchor::Url`, wrapped in `GatedSession`. Its `do_update` can be held until cancel, like a server that has not answered the REPORT yet. `StaticConnector` in `support.rs` is left unchanged.

- Checkout of ROOT/trunk, then update to HEAD, a multi-path update (giving a mixed range), and an update back to r1. Progress is checked to be monotonic with the right phases.
- Switch to `branches/b1`, update on the branch, then switch back to trunk at r1. Cross-repository targets are refused: another repo on the same host, a prefix trap (`realx`), another host.
- Cancel while waiting for the server returns `Cancelled` in under 1 s. A later `update` finishes the interrupted checkout.
- A checkout cancelled half-way (cancelled from the progress callback after the first path) is finished by `update`.
- Unsupported requests: V32, sparse, set_depth, and an update path that escapes the working copy.
- A connector that cannot anchor the session is refused.
- A context that is already cancelled does nothing.

## Gates

- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets -- -D warnings`: pass
- `cargo clippy -p opensvn --features real-engine --all-targets -- -D warnings`: pass
- `cargo test -p opensvn`: 249 passed, 0 failed
- `cargo test -p opensvn --features opensvn/real-engine`: 325 passed, 0 failed (7 new sync tests + 2 progress unit tests)

`remote_tests::working_copy_operations_report_that_they_are_not_wired` was removed: it asserted that checkout is not wired.

## Remaining

- `switch` always switches the whole working copy (the facade has no path argument) at `Depth::Unknown`.
- `ignore_externals` and `store_pristine` are ignored: format 31 always stores pristine copies, and externals are not supported.
- Not checked against real svn (oracle). The behaviour comes from `svn-client`, which has its own oracle tests.
- The real `HttpConnector` (WP2) must implement `connect_with` with `Anchor::Url` through `HttpRaSession::reparent`. Until it does, checkout/update/switch fail with the clear "connector opened a session at ... instead of ..." error.

## Review fixes (REQUEST_CHANGES on #95)

### Blocking

1. **Externals are not fetched in the MVP.**
   - Engine: `engine/real/sync.rs` documents in the module docs and on `do_checkout`/`do_update` that externals are never fetched (`svn --ignore-externals`), whatever `ignore_externals` says. `CheckoutPlan` docs say the same.
   - New `EngineFacade::capabilities() -> EngineCapabilities { externals }`. Real returns `false`; Fake returns `true` and keeps accepting the flag.
   - New IPC command `engine_capabilities`, wired through: command names, registry, capability allowlist, permission toml, bindings, and doc 03.
   - Test `sync_tests::externals_are_never_fetched`: with `svn:externals` set on trunk, a checkout with `ignore_externals` false or true, followed by an update, never creates `ext`. The property itself is still checked out.
   - FE: the checkout dialog no longer has an "Ignore externals" checkbox and always sends `ignoreExternals: false`. When `engineCapabilities().externals === false` it shows "Externals are not fetched yet." with a link to issue #97.
   - Issue #97 was opened: "Support svn:externals (fetch on checkout/update, status, commit)".
2. **`store_pristine == false`** is refused with `WcFormatUnsupported` and a message about pristine copies. Nothing is created on disk. The assertion is in `unsupported_requests_fail_with_user_facing_errors`.

### Non-blocking

- **Commit between paths.** Test `a_commit_between_paths_leaves_every_path_at_the_same_revision`: `GatedSession` commits r4 after the first per-path drive. Both paths end at r3 and nothing reaches r4.
- **Session URL check.** `same_location` compares scheme, host and port as the parser normalizes them, i.e. case-insensitively. The repository-root part of the path is compared case-insensitively (the VisualSVN rule). The path below the root is compared exactly. Unit tests cover case, port, scheme, a prefix trap, and a repository at the server root.
- **Cancelled or failed checkout from the UI.**
  - `WcInfo.incomplete` is true when any op_depth-0 node has presence `incomplete`. It is exposed as `WorkingCopy.incomplete` and `WcEntry.incomplete`.
  - On a checkout error, `WcRegistry::add_unfinished` registers the partial working copy, but only when an admin folder sits directly in the target and the engine's root is exactly the target, so an enclosing working copy is never registered. Otherwise the old `discard_if_empty` runs.
  - The repository picker shows "Incomplete — Update to finish".
  - The list refreshes on `op://failed`.
  - The cancelled message tells the user where to find the partial working copy.
- **Progress without a total.** The checkout dialog's `<progress>` is indeterminate (no value or max) and the text reads "N items · path". In the activity row, a running operation with no percent shows an indeterminate bar.

### Gates (local)

- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets -- -D warnings`: pass
- `cargo clippy -p opensvn --features real-engine --all-targets -- -D warnings`: pass
- `cargo test -p opensvn -p opensvn-ipc`: pass. opensvn lib 251; opensvn-ipc 4 + 6 + 18 + 3.
- `cargo test -p opensvn --features opensvn/real-engine`: 331 passed
- `gen:ipc`: no drift
- FE:
  - `pnpm install --frozen-lockfile`, `lint`, `typecheck`, `lint:safety`: pass
  - `test`: 28 files, 203 passed
  - `build` + `check-dist`: pass

### Remaining

- Real `HttpConnector` `Anchor::Url` support (WP2), as noted above.
- The incomplete registration was tested at the registry level with the FakeEngine plus a planted `.svn`. The real engine's incomplete flag was tested in `sync_tests`. No app-level test with the real engine exists, because the app test fixture only runs the FakeEngine.
