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
- `layout.rs`
  - `EngineDirs` moved here from `commands/remote.rs`. It now borrows the engine and lists with
    `OpContext::background()`, so detection never prompts for credentials or certificates.
  - `may_be_in_layout(repo_root, url)`: a cheap URL-shape check (`trunk`, `branches/<x>`,
    `tags/<x>` at the root or one project level down). Folder-style URLs skip the server round
    trips entirely.
  - `project_layout()`: the `/{project}/…` patterns, shared with detection.
- `wc/registry.rs`: `describe` fills `branch` through `branch_of`:
  - Skipped when the URL shape does not fit, or when the stored layout is `None`.
  - Uses the existing `layout::detect` and caches the result per working copy **and URL**. A switch
    changes the URL, so the next `wc_list` re-detects. This keeps the toolbar in step with
    `wc://revision-changed`, which already carries the URL (#130).
  - Bounded by a 5 s timeout. A failure (offline, no credentials, untrusted certificate) gives
    `None` and is retried after 60 s. Only the wc id and the error code are logged.
- `commands/remote.rs` `branches_list`: when no layout is stored and the root lists nothing, it
  retries with the per-project layout. Without this, a project-level layout would get no list.

### Frontend (`apps/opensvn/src`)
- New `features/remote/branch/branch-switch-list.tsx`, shown in the Branch popover only when
  `wc.branch` is set:
  - Lists the trunk, branches and tags of the same project through `branchesList`, without the
    current one.
  - With local changes, a click first asks "N local change(s) will be carried over to X" with
    **Carry changes and switch** / **Cancel**. The switch uses the existing `switch` command.
  - The popover closes after the switch starts. "Switch…" (the URL field) is still there for any
    location.
- `app-shell.tsx`: the location popover is now controlled; it passes the changed-file count.
- Strings added in en and vi.

## Tests
- Conformance (fake and real engine): `branch_detection_follows_a_switch`. It checks out trunk,
  detects Trunk, lists branches, switches to `branches/feature-y` and detects it, then switches to
  `lib-copy` (outside the layout) and gets no branch.
- `layout::tests::only_urls_shaped_like_a_layout_are_worth_detecting`.
- `commands_flow::a_switch_at_the_same_revision_announces_the_new_location` now also asserts
  `branch` trunk → feature-x after the switch.
- `commands_flow::a_repository_without_a_layout_shows_no_branch` (stored layout `None`).
- FE `branch.test.tsx`: the list and click-to-switch, the carry/cancel prompt, and no picker
  without a branch.
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
- **TLS default-deny:** detection uses `OpContext::background()`, which never prompts. An untrusted
  certificate fails the detection (no branch shown); nothing bypasses trust.
- **Credentials in logs:** the failure log has the wc id and error code only, no URL, no user name.
- **svn 1.8–1.15:** no change to `wc.db` or its format. Detection only lists directories.

## Follow-ups
- A custom layout stored by the user (for example `/code/main`) is not recognised by `detect`,
  so such working copies show Location and the URL field. Detection could read the stored
  patterns.
- `branches_list` lists the whole repository. For very large `branches/` folders, the popover
  could page or filter.
