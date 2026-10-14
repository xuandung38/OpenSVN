# App UX follow-ups (feat/app-ux-followups)

Date: 2026-10-14. From origin/main 44c77f5.

1. **Startup failure**: `run()` reports a failed start through `report_startup_failure`:
   `tracing::error!` (when logging is up), stderr, and a native error dialog (`rfd`, already a
   dependency; a panic of the dialog layer without a desktop session is caught).
2. **Demo engine banner**: `EngineCapabilities.demo` (IPC + bindings + contract doc). The fake engine
   reports `true`; `engine_capabilities` masks it in debug builds (`visible_capabilities`). The FE
   shows a permanent, non-dismissible danger banner (`app/demo-engine-banner.tsx`, en/vi strings).
3. **OPENSVN_ENGINE**: an unknown value is a startup error in debug builds (shown by item 1), a
   warning in release builds as before.
4. **needs-cleanup phase**: notice phases are now defined in `engine::context` and bypass the
   progress throttle (`ProgressUpdate::is_notice`), so a notice is never coalesced away. The FE event
   bus turns `needs-cleanup` into the working copy's `needs-cleanup` banner ("Run cleanup").
5. **W150002**: add and commit auto-add report each skipped nested working copy as a
   `nested-wc-skipped` notice (still logged); the FE shows a toast.
6. **Doc nit**: the svn config follower's doc says when it stops (settings dropped, or the first
   settings change after the engine was dropped).

Also: `repo-manager.test.tsx` "footer and File menu events..." failed on main on this machine
(4 of 4 runs): the menu event right after opening the picker by keyboard counted as that key's
echo on a fast machine. `Date.now` is now frozen for the whole test (9/9 in 3 runs).

## Tests
Rust: `lib.rs` (fatal unknown engine in debug, startup failure reaches the dialog), `ops/tests.rs`
(notices are never throttled away), `commands_repo_manager.rs` (demo masked in debug),
`commit_tests.rs` (W150002 notices from auto-add and add), `serde_shape.rs`.
FE: `ipc.test.ts` (notices: banner, not progress), `wc-dialogs.test.tsx` (demo banner shown /
absent, needs-cleanup banner, nested-WC toast).

## Gates
See the PR body.
