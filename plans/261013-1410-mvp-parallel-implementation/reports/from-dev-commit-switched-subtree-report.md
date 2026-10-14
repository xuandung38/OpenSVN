# Commit with changes below a switched subtree (#111)

Branch `fix/commit-switched-subtree`, from origin/main e879ece. Date: 2026-10-14.

## Change
- `svn_client::commit::commit_anchor(db, targets, opts, within)`: read-only harvest, then the
  session anchor svn's `svn_client__condense_commit_items` would pick: the longest common
  ancestor of `within` (the working copy URL path) and every committable's repository path,
  moved to the parent while an item sits exactly there and is not a directory with only
  property changes. Never above the repository root. Changes inside the working copy URL keep
  the anchor at the working copy URL (path-based authz on the subtree keeps working).
- `commit()` refuses, before any server transaction, a harvest that the session prefix cannot
  express (`WcInvalidOperation`), instead of failing in `build_trie` after the transaction was
  opened.
- RealEngine `do_commit`: after the selection is resolved (auto add/remove), asks
  `commit_anchor`; when it differs from the working copy URL, reconnects at the anchor
  (`ConnectOptions::at_url`), authenticates (no lock held) and commits with that prefix. Editor
  paths, the MERGE and lock-token paths all follow `session_prefix`.

## Tests
- `svn-client` oracle vs svn 1.15 (`commit_vs_svn.rs::commit_with_a_switched_subtree_matches_svn`):
  trunk working copy with `sub` switched to `^/branches/b`, edits on both sides plus an add on
  the branch, one commit. Through a session at trunk: refused, no revision, rows unchanged;
  `commit_anchor` returns the root; through the root session: same revision as svn, identical
  working copies (files, NODES, ACTUAL, status) and identical repositories.
- `svn-client` unit tests for the anchor rule.
- RealEngine (`commit_tests.rs`): switched `src` to a branch, edits on both sides, commit of the
  root goes through, repository and BASE revisions checked. Fails without the engine change.
- `tests/real_engine_dav.rs::commit_with_changes_below_a_switched_subtree`: the same over a local
  Apache 2.4 + mod_dav_svn 1.14.3 (HTTPS, Basic auth): passes.

## Notes
- The DAV test switches through a session anchored at the folder: switching a sub-path through
  a session at the working copy root hits the separate report-path bug (#122).
