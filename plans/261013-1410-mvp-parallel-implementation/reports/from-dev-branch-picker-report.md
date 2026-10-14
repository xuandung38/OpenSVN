# Report: Branch picker and working-copy branch (#129)

- Branch: `fix/branch-picker-129`
- PR: #136 (against `main`)
- Base: `origin/main` 9425158, with `origin/test/e2e-flows` (#130, still open) merged in.

## Problem
`WorkingCopy.branch` was always `None` (`wc/registry.rs`). Because of that, a trunk/branches/tags
repository showed "Location /trunk" with "Copy to new location…" and a free-text Switch dialog.
`layout::detect` and `branches_list` were never used for the working copy.

## Changes

### Backend (`apps/opensvn/src-tauri`)
Revised after the review of PR #136. The first version detected the layout on the server inside
`describe()`, so `wc_list` could wait up to 5 s per unreachable working copy.

- `layout.rs`
  - `branch_from_url(repo_root, url, stored_layout)`: the branch from the URL and the stored layout
    only, with **no network**.
    - `Layout::None` gives no branch.
    - A stored `Layout::Custom` is honoured, including `{project}` patterns.
    - No stored layout or `Auto` means the trunk/branches/tags convention, at the root or one
      project level down.
    - Matching is case-sensitive, like Subversion.
    - Names are percent-decoded for display; the URL stays encoded.
  - `layout_near(repo_root, url)`: the layout to list branches with when none is stored. It is the
    root convention, or only the project folder the URL is in.
  - `EngineDirs` moved here from `commands/remote.rs` and lists with `OpContext::background()`.
    `detect` (the `layout_detect` command) also decodes names now.
- `wc/registry.rs`
  - `describe` fills `branch` through `branch_from_url`, so `wc_list` reads only local data, as
    before this PR.
  - `entries()` reads the engine info only and never computes the branch.
  - A switch changes the URL, so the next `wc_list` after `wc://revision-changed` shows the new
    branch.
- `commands/remote.rs` `branches_list(repo_root_url, at_url)`
  - Uses `OpContext::background()`, so it never raises a credential or certificate prompt.
  - A stored layout wins. Otherwise it lists with `layout_near(at_url)`, and never sweeps every
    top-level folder.

### Frontend (`apps/opensvn/src`)
- New `features/remote/branch/branch-switch-list.tsx`, shown in the Branch popover only when
  `wc.branch` is set:
  - Lists the trunk, branches and tags of the same project through `branchesList(root, branch.url)`,
    without the current one.
  - The query runs only when `wc.branch` is set (`enabled`), so a folder-style working copy never
    lists branches.
  - With local changes, a click first asks "N local change(s) will be carried over to X" with
    **Carry changes and switch** / **Cancel**. The switch uses the existing `switch` command.
  - The popover closes after the switch starts. "Switch…" (the URL field) is still there for any
    location.
- The Create branch/tag dialog no longer lists branches in Location mode.
- `branch-model.ts`: the convention regexes are case-sensitive, like the backend.
- `app-shell.tsx`: the location popover is now controlled; it passes the changed-file count.
- Strings added in en and vi.

## Tests
- Conformance (fake and real engine): `branch_detection_follows_a_switch`.
  - It checks out trunk, detects Trunk and lists the branches.
  - It switches to `branches/feature-y` and detects it, then switches to `lib-copy` (outside the
    layout) and gets no branch.
  - At each step, the offline `branch_from_url` must agree with the server-side `detect`.
- `layout::tests::branch_from_url_follows_the_convention_without_the_server`: the convention,
  project level, case, percent-decoding, `None`, a custom layout, and URLs outside the
  repository.
- `layout::tests::layout_near_limits_listing_to_the_project_of_the_url`.
- `commands_flow::a_switch_at_the_same_revision_announces_the_new_location` now also asserts
  `branch` trunk → feature-x after the switch.
- `commands_flow::a_repository_without_a_layout_shows_no_branch` (stored layout `None`).
- FE `branch.test.tsx`
  - The list and click-to-switch, including the `branchesList` arguments.
  - The carry/cancel prompt.
  - No picker and **no `branchesList` call** without a branch. This test fails without `enabled`.
- e2e 10.1 now expects "Branch trunk" instead of "/trunk" in the toolbar.
- e2e 10.5 now goes through the picker: Branch trunk → feature-x → Carry changes and switch → local
  change kept, toolbar shows "Branch feature-x". It then switches back to trunk with the URL field.
  The old test clicked `/Location/`, which no longer exists for a layout repo.

## Gates
See the PR body for the counts on the final head.

## Self-review
- **Data loss:** nothing is deleted. A switch with local changes now asks first; it is the same
  `switch` operation as before, and svn keeps the local changes.
- **Path escape:** only URLs are involved, no local paths.
- **TLS default-deny:** the toolbar branch needs no server. `branches_list` uses
  `OpContext::background()`, which never prompts. An untrusted certificate fails the list (the
  popover says so and keeps Switch…); nothing bypasses trust.
- **Credentials in logs:** nothing new is logged apart from a failed read of the stored layout.
- **svn 1.8–1.15:** no change to `wc.db` or its format.
- **Latency:** `wc_list` and `wc_entries` make no server calls again.

## Follow-ups
- A working copy of a folder-style repository whose path happens to contain `trunk` (for example
  `/docs/trunk`) shows a branch, because no server check is made. Setting the layout to None in
  the repository settings hides it.
- For very large `branches/` folders, the popover could page or filter.
