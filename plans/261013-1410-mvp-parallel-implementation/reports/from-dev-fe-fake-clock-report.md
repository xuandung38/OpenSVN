# FE tests: migrate remaining real-clock tests to installFakeClock

Branch: `test/fe-fake-clock` (from `origin/main` @ f4f712b)

## Changes
- Migrated to `installFakeClock()` (`user()` replaces the `userEvent` default; findBy/waitFor budgets run on the fake clock):
  `features/remote/{auth,blame,branch,browse,lock}/*.test.tsx`, `features/remote/remote-pane.test.tsx`.
- auth.test.tsx: the two real `setTimeout(r, 20)` waits for the event bus became `vi.advanceTimersByTimeAsync(20)` (same 20 ms, now simulated).
- Per-file hang guard `vi.setConfig({ testTimeout: 120_000 })` (same pattern as wc-changes/wc-dialogs). It is needed because
  the first loaded runs failed only on Vitest's wall-clock per-test limit (5 s / 20 s), never on a query budget.
  remote-pane's per-test `20_000` was dropped so the file guard applies. history.test.tsx guard 30 s -> 120 s.
- `components/list/keyboard.test.tsx`: hang guard only (it has no findBy/waitFor; it timed out at 5 s under load).
- No assertion was changed, weakened or skipped.

## Skipped (touched by PR #81 feat/12c-repo-manager)
`features/remote/settings/settings.test.tsx` (still real-clock), `features/wc/repos/repo-manager.test.tsx`, `features/wc/wc-dialogs.test.tsx`.

## Verification
- Load: 4x `yes > /dev/null` on 4 vCPUs, `pnpm -C apps/opensvn test` x3 at default config.
  - Final: run1 179/180, run2 179/180, run3 180/180. The only failure (both times) is
    `settings.test.tsx > Save is disabled until something changes...` (5 s timeout), which is the excluded #81 file.
    All 23 other files passed 3/3.
  - Before the hang-guard fix: 6 and 8 failures, all `Test timed out in N ms`.
- Non-vacuity: renamed the findBy target in lock.test (`"Steal lock"` -> `"Steal lockX"`) and the waitFor target in
  branch.test (`copyLocation` -> `copyLocationX`); both failed (`Unable to find role="button"`, `expected false to be true`); reverted.
- Gates (unloaded): lint ok, typecheck ok, lint:safety ok, test 24 files / 180 tests ok, build + check-dist ok.

## Remaining
- After #81 lands, migrate `settings.test.tsx` the same way; until then the full suite is not 3/3 under heavy load.
