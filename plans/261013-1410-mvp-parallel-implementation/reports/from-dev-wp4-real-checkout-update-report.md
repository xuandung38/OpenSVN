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
