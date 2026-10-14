# svn:externals — dev report (2026-10-14)

Branch `feat/svn-externals`, PR #118. Closes #97, refs #99 item 3.

## What was built
- **`svn-client::externals`** (new module). It runs after every `checkout`, `update` and
  `switch` when `ClientCtx::externals` holds a `SessionOpener`. With `None` the behaviour is
  `svn --ignore-externals`.
  - `parse.rs`: the parser follows svn 1.15 `svn_wc_parse_externals_description3`:
    - old format and new format; `-r N`/`-rN`, `URL@PEG`, a trailing `@` as escape;
    - quoting and backslash escapes; comments;
    - a bad line or a duplicate target fails the whole value.
    - Targets are validated as untrusted server text (`RelPath::from_server`): no `..`, no
      absolute path, no `.svn` alias, no device names.
  - `resolve_url`: `^/` (it may go above the repository root, e.g. `^/../repo2`), `../`
    (relative to the defining directory), `//` (same scheme), `/` (same scheme, host and port).
  - `dir.rs`: directory externals. Each one is a separate working copy:
    - checked out when missing;
    - updated when it is the same URL;
    - switched when the URL changed within the same repository;
    - replaced when it moved to another repository, but only if this WC registered it and the
      old copy has no local changes.
    - Then it is registered in `EXTERNALS`, with the pinned peg and operative revisions as svn
      writes them.
  - `file.rs`: file externals. The file is fetched with `get_file`/`stat` and applied through the
    regular `UpdateEditor` as a synthetic one-file switch drive. Merge, conflicts and
    `file-install` therefore behave as for any other file. The BASE row is marked
    `file_external = 1` and the `EXTERNALS` row is written in the same transaction as the file.
    The external must come from the same repository and sit below a versioned directory of the
    WC (as in svn).
  - `remove.rs`: removes externals whose definition is gone.
    - A directory external with any local change keeps everything, including `.svn`. A nested
      external with changes counts as a change too.
    - An unmodified one loses its versioned files and its `.svn`. Unversioned and ignored items
      stay. Each file is checked again right before it is deleted.
    - A file external becomes svn's `not-present` marker. Its file is deleted only when it is
      unmodified. A conflicted one is refused.
    - Emptied unversioned parent directories are pruned, as svn does.
  - Externals nested in externals are processed recursively. They are bounded three ways
    (review on #118):
    - cycle detection refuses an external whose URL is, or contains, its defining directory
      or any directory whose definition led to it (`(uuid, repos_relpath)` chain);
    - one operation processes at most `MAX_EXTERNALS = 1000` externals across all levels
      (`ClientConfig::max_externals`);
    - nesting stops at `MAX_NESTING = 8` levels.
  - A failure is reported per external in `UpdateSummary::externals` (`ExternalReport`) and does
    not stop the other externals. Cancellation stops everything.
- **Update core fixes** needed by externals:
  - The reporter skips file externals.
  - The parent's drive never touches a file external.
  - `bump_revisions_post_update` leaves non-root file externals alone.
  - A switch target that is a file gets the switch path.
  - `relocate_subtree` is now idempotent. It moves only the nodes whose path is still the old
    prefix plus their WC suffix. Before, `svn switch` from `lib` to `lib/sub` recorded
    `lib/sub/sub`, a bug that existed before this PR.
- **svn-wc**:
  - `WcDb::externals_definitions_under`: ACTUAL wins over BASE, present dirs only, like
    `svn_wc__externals_gather_definitions`.
  - `WcTxn::{external_add, external_remove, mark_file_external}`, `NewExternal`.
- **RealEngine**:
  - `engine_capabilities.externals = true`.
  - `ConnectorOpener` opens every external session through the engine's `SessionConnector` with
    the operation's `ConnectOptions`. TLS default-deny and sign-in apply unchanged, also on other
    hosts.
  - Checkout honours `ignore_externals`.
  - Warnings are logged and sent as progress samples with phase `external-warning`; the
    operation still succeeds, as svn only warns.
- **FE**: the checkout dialog shows the "Ignore externals" checkbox again when
  `engine_capabilities.externals` is true. The old note stays for engines without externals.

## Tests
- `crates/svn-client/tests/externals_vs_svn.rs`: oracle tests against svn 1.15.0, built from
  source. Each test replays two sibling repositories into fakes. It compares files, the NODES
  rows of every WC, the `EXTERNALS` rows, the file-external columns, the ACTUAL rows and
  `svn status`. Cases:
  - checkout with every URL form both clients share: pinned, pegged, file, other repository,
    `../`, nested;
  - update that adds, switches and removes externals (modified dir kept; unversioned-only dir
    partly removed; unmodified dir with a nested file external removed);
  - a modified file external that loses its definition;
  - a conflicting change in a file external;
  - a merge in a file external;
  - local (uncommitted) definitions;
  - `--ignore-externals`;
  - svn ≥ 1.14 reading our WC. Run with svn 1.14.3: status, cleanup and info all pass.
- `crates/svn-client/tests/externals_fake.rs` covers:
  - absolute, `/` and `//` URLs, another host through the opener, the old format;
  - one failure does not stop the others;
  - `..`, absolute and `.svn` targets refused;
  - symbolic links on the way or as the target refused (`UnsafePath`), nothing written outside;
  - versioned items and foreign WCs in the way left alone;
  - a file external from another repository refused;
  - the nesting limit;
  - cancellation;
  - a kind change;
  - removal when the defining directory was deleted on the server;
  - a shallow update leaves externals alone;
  - an update of a file external alone is refused;
  - an explicitly committed file external stays a file external (also passes merged with
    #105).
- RealEngine (`sync_tests`):
  - checkout and update fetch externals unless they are ignored; status shows `X`;
  - an external on another host goes through the connector; a TLS refusal only warns.
- FE: the checkbox appears, and checking it sends `ignoreExternals: true`.

## Deliberate differences from svn 1.15 (documented in `externals/mod.rs`)
1. A modified file external whose definition goes is kept as an unversioned file. svn deletes it.
2. A local change in a file external is merged against its old text. svn merges against an
   empty base, so every local change conflicts and `.rOLD` is empty.
3. The inherited-properties cache of a file external is `()`. svn caches the ancestors' props;
   the RA layer has no call for directory properties yet.
4. `-r {DATE}` is refused. With a peg revision that differs from the operative revision, the URL
   is used at the operative revision (no history tracing).
5. Updating a file external on its own is refused; update its defining folder.

## Follow-ups (not in this PR)
- Surface `external-warning` samples in the UI (activity panel / toast).
- `status -u` does not descend into externals (svn runs a remote status per external).
- Local operations on file externals (delete, move) should be refused like svn does. Commit-side
  protection is in #105.
