# Update of sub-paths over DAV (fix/update-subpath-report)

Date: 2026-10-14. Independent of #116 (branched from origin/main e879ece; no dependency).

## Root cause
`svn_client`'s reporter describes the working copy with session-relative paths (`sub/dir` first,
then `sub/dir/f`), which the fake RA understands. svn's wire protocol, as svn's crawler and
ra_serf use it, anchors the edit at the target's parent (`S:src-path`), names the target by its
last component (`S:update-target`) and reports paths relative to the target (`""` first).
`svn-ra` passed the session-relative paths through and rejected multi-component targets, so
`update(paths=[sub/dir | sub/file | file])` on a session anchored at the working copy root failed
with "update report must start with set_path for the root" against mod_dav_svn.

## Fix (svn-ra, contract unchanged)
- `request_options`: `S:src-path` = session path + parent of the target, `S:update-target` = its
  last component (single-component and root targets unchanged).
- `CollectingReporter`: report paths rebased to the target (`""` = target); a path outside the
  target is refused (`Internal`) before any request.
- `EventBridge`: server paths (relative to the anchor) rebased to session-relative ones; the root
  and the directories down to the anchor are opened around the server's drive and closed after
  it, so the editor sees the same drive as from the fake session.
- `svn-client` `drive_update`: refuses (clear `WcInvalidOperation`) when the target's parent is
  switched away from where the session URL says it is, since the wire anchor would be wrong.
- `ReportFn` docs state the session-relative contract.

## Tests
- `svn-ra` `ops_update.rs`: nested target `a/b/c` (src-path, update-target, entry `""`, missing
  `x`, editor paths under `a/b/`); report paths outside the target refused; switch test reports
  session-relative paths.
- `svn-client` oracle vs svn 1.15: `update_of_nested_targets_matches_svn` (`src/lib`,
  `docs/guide.txt`).
- Conformance scenario `update_of_sub_paths` (folder, nested file, top-level file; mixed revision
  range; clean status) in the shared suite: fake engine, real engine over the fake repository,
  and `tests/real_engine_dav.rs`. Against a local Apache 2.4 + mod_dav_svn 1.14.3 (HTTPS, Basic)
  it failed before the fix with the error above and passes after; the whole DAV suite (17) passes.

## Gates
See the PR body.

## Not covered
A switched update target (svn reports it with `set_path("")` + `link_path("")`) is not handled
specially; the reporter emits one `link_path` for it, which the wire refuses. Unchanged from
before; switched parents are refused explicitly.
