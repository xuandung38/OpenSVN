# End-to-end flows 10.1–10.5 (test/e2e-flows)

Date: 2026-10-14. From origin/main 44c77f5, merged with origin/main 56e4ea1.

## What
`apps/opensvn/e2e/` drives the real app (debug build, `--features real-engine`) through
tauri-driver 2.1.0 + WebKitWebDriver (W3C WebDriver over `fetch`, no new npm dependencies;
`node:test`). Each test gets a scratch repository on Apache + mod_dav_svn (HTTPS, Basic
`tester`/`tester-pass`) and a fresh app profile (`HOME`/`XDG_*`). Controls are found by role and
accessible name, results are checked on disk and with the `svn` client.

- `pnpm e2e` (`e2e/run.sh`): builds, runs under `xvfb-run` when there is no display.
- Server: `OPENSVN_COMPAT_LOCAL_URL` + `OPENSVN_COMPAT_LOCAL_REPOS`, else the compat container
  `opensvn-compat-svn15-server-1` (`cargo xtask compat up`).
- CI: job `e2e` in `.github/workflows/compat.yml`, schedule + workflow_dispatch only.
- Doc: `apps/opensvn/e2e/README.md`.

## Flows (local Apache 2.4, svn 1.14 client for seeding; 2 runs, ~1 min each)

| Flow | Result |
|---|---|
| 10.1 checkout: trust certificate, sign in, empty Changes | pass |
| 10.2 edit → diff → Ctrl+Enter commit → list empties | pass |
| 10.2 out of date → Update merges → commit | pass |
| 10.3 conflict banner → resolver (Mine) → banner gone, markers removed | pass |
| 10.4 needs-lock: lock → writable → unlock → read-only → lock → commit releases | pass (after fix 1) |
| 10.4 lock held by someone else → owner + Steal | `todo`: #127 |
| 10.5 switch to `branches/feature-x`, local change carried, toolbar updates | pass (after fix 2), via URL field: #129 |
| History + Blame | pass |

## Fixes (small, with regression tests that fail without them)
1. Real engine `lock`/`unlock` of working copy files did not change permissions (noted as "not
   handled yet" in `locks.rs`): a `svn:needs-lock` file stayed read-only after locking. Now synced
   right after the lock row changes (same inputs as the `sync-file-flags` work item, but direct and
   best effort, so a failure never leaves queued work that would block the working copy).
   Test: `commit_tests::a_needs_lock_file_is_writable_only_while_locked`.
2. `wc://revision-changed` fired only when the revision changed: a switch at the same revision
   left the toolbar on the old location. The URL is now part of the announced state.
   Test: `commands_flow::a_switch_at_the_same_revision_announces_the_new_location`.

## Issues opened
- #127 lock held by another user: dialog closes before the server's 423; no owner, no Steal;
  `OpenLockDialog` action unhandled in the FE.
- #128 unmodified needs-lock / locked files are not listed anywhere: no "Lock to edit" before
  editing, no Unlock of an unmodified locked file.
- #129 `WorkingCopy.branch` always `None`: no "Branch ▾" picker, Switch is a URL text field.

## Notes
- Running the file with `node --test` puts the whole file under `--test-timeout`, so `run.sh` runs
  it as a plain script.
- The e2e job has not run on GitHub yet (CI is skipped for now); it reuses the compat image build.
