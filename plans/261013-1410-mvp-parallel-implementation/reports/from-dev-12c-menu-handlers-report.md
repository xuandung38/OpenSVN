# 12c: fixes for review 5507856351 on #81 (menu handlers, docs, is_deceptive drift)

Branch `feat/12c-repo-manager`. Includes a merge of main `802822f` (#76).

## 1. Every menu item has a handler
- `MENU_EVENTS` (`src/ipc/events.ts`) now lists all 60 UI-handled menu ids. `MENU_ACTIONS` (`use-wc-shortcuts.ts`) maps each one to an action, and its keys equal `MENU_EVENTS` (vitest).
- New handlers include:
  - `edit.find` → focus the filter; `view.diff-mode` / `view.hide-whitespace` → toggle; these go through `ui-commands.ts`, so echo suppression covers them.
  - `file.close-wc` (`wc_close`, then no working copy is current); `file.recent` (picker).
  - `edit.copy-path` / `edit.copy-url`; `view.sidebar`; theme items.
  - `wc.update-to` / `discard-all` / `cleanup` / `reveal` / `terminal` / `editor`.
  - `branch.new` / `new-tag` / `show-on-server`.
  - `sel.unlock` / `changelist` / `ignore` / `resolve` / `rename` / `delete`.
  - `window.conflicts`; `help.shortcuts` (new shortcuts dialog).
- Items that belong to later phases show a "not available yet" toast instead of doing nothing: merge, compare, layout, sparse, properties, the view toggles, check app updates, docs, logs, report.
- Rust handles these itself (`native_action`), and they emit no event: zoom in, zoom out, actual size (webview `set_zoom`), and Exit.
- Rust tests:
  - Every spec item on every OS and layout is handled by exactly one of the UI or Rust.
  - Every UI id is a forwarded item.
  - The UI `SHORTCUTS` keys equal the menu accelerators, which echo suppression depends on.
  - Zoom steps are tested.
- Vitest covers:
  - edit.find focuses the filter.
  - diff-mode echo in both orders, plus a request more than 400 ms later, which still runs.
  - hide-whitespace, the shortcuts dialog, a not-yet toast, copy URL, the sidebar toggle, reveal and close-wc.

## 2. Docs
Doc 03 §3.7 is rewritten for the single menu and the three item kinds. §3.5 `menu_set_layout` mentions zoom/Exit. In `adapter.ts`, the example now reads `menu://wc.commit`.

## 3. is_deceptive
U+0890 and U+0891 are added in Rust and the FE. A Rust test generates `src/features/wc/repos/deceptive-ranges.json`; regenerate it with `OPENSVN_UPDATE_FIXTURES=1 cargo test -p opensvn display_text`. The test fails if the fixture is stale. `repo-model.test.ts` checks every non-control code point: `FORMAT_CHARS` must match the fixture exactly.

## 4. Nits
- The `dunce` comment is moved.
- A Rust test covers a symlinked item: the default app refuses with the reveal action, reveal then refuses with the "symbolic link" message, and nothing is recorded. A link through a folder outside the WC is refused without the reveal fallback.
- A vitest test covers `openWithDefaultApp` for the same case and checks that the user gets the reveal refusal.
- Follow-up issue #87 is open for editor/terminal following symlinks.

## Gates (local)
| Gate | Result |
|---|---|
| cargo fmt --all --check | pass |
| clippy --workspace --all-targets -D warnings | pass |
| clippy -p opensvn --all-targets --features real-engine -D warnings | pass |
| cargo test -p opensvn -p opensvn-ipc -p svn-types | pass (opensvn lib 249) |
| cargo test -p opensvn --features real-engine | 313 pass, **1 fail**: `engine::real::tests::edit_tests::copy_reports_what_the_repository_cannot_do` (PathNotFound vs ServerUnsupported). It **fails the same way on origin/main 802822f** (checked in a detached worktree), so it comes from #76, not from this PR. |
| gen:ipc | no drift |
| FE install / lint / typecheck / lint:safety | pass |
| FE test | 199/199 |
| FE build + check-dist | pass |

## Remaining
- The real-engine failure above needs a fix on main (#76 area).
- The not-yet menu items wait for their phases.
- Native menu items cannot yet be disabled by context: a selection-scoped item with no valid selection does nothing.
