# Engine hardening follow-ups: dev report

Branch `fix/engine-hardening-followups` (from origin/main edce2f0), PR #115.
Closes #87, #98, #101, #104; refs #99 (items 1 and 4).

## Done

### #87 — editor/terminal and symlinks
- `os_integration::open_external` with `Editor`/`Terminal` now resolves the path through `reveal_target`.
  This is the same rule as reveal: no symlink component below the WC root, and the canonical target must stay inside the canonical root.
  The editor gets the canonical path.
- Unix tests cover a final-component link, an intermediate link and links that point inside the WC.
  In each case the recording opener records nothing.
- Doc 03 (`open_external` row) is updated.

### #98 — diff_wc and fake/real alignment
1. `unversioned_path` uses `svn_types::is_any_admin_dir_name`.
   On Linux, a test creates `.svn.`, `.svn `, `SVN~1`, `_svn` and `.SVN` folders and checks that each is refused.
2. `open_seen_file` opens files with `O_NOFOLLOW | O_NONBLOCK` (Unix) or `FILE_FLAG_OPEN_REPARSE_POINT` (Windows).
   It then verifies the handle: a regular file with the same dev/ino as the earlier `lstat`; on Windows, no reparse attribute.
   Both the versioned and the unversioned read paths use it.
   A unit test swaps the file for a symlink, then for another file.
   `libc` (=0.2.190, already in the lockfile) is added as a unix-only dependency of the app.
3. FE mocks (`data.ts`, `remote-data.ts`, `wc-mock-commands.ts`) send `token: null`, and a vitest checks it.
4. FakeEngine refuses `\n`/`\r` in ignore patterns of both scopes. The check is in the shared conformance suite, so both engines run it.
5. FakeEngine diff against a revision sends property rows: the file's props at that revision against the working props, without `svn:entry:`/`svn:wc:` props.
   `a_diff_against_a_revision_reads_the_left_side_from_the_repository` compares the real and fake prop rows.

### #101 — commit journals
1. `recover_wc_commits` reports a journal of an unknown format that names no `wcroot` in `needs_user`, for every WC it may belong to.
   The app therefore reports it in `needs_user_action`.
2. Owner hint: `JournalStore::new_id_for(wcroot)` gives `commit-<sha1(wcroot)[..12]>-<id>.json`, and `commit()` uses it.
   `JournalStore::may_belong_to(path, wcroot)` decides from the file name only:
   - a matching hint, or no hint at all (old names), counts as "may belong";
   - a hint is exactly 12 lowercase hex digits followed by `-`, and old ids start with 16 hex digits, so they never look like a hint.
   `recover_wc_commits`, `finalize_pending_for_wc` and the app's `scan_journals` skip files of other WCs.
3. Corrupt journals are aligned with the "unreadable blocks" rule.
   - A corrupt file that may belong to the WC blocks commits (`WcNeedsCleanup`).
   - Recovery on open sets it aside and reports it once, and after that it no longer blocks.
   - A corrupt file carrying another WC's hint is left for that WC to handle.
   - The rule is documented in svn-client `recovery.rs` and in doc 02.
4. The network part of recovery (unattended session plus `recover_wc_commits`) runs under `tokio::time::timeout(RECOVERY_NETWORK_TIMEOUT = 60s)`.
   Stopping there is safe because futures only stop at network awaits, and journal writes and finalize are synchronous between them.
   On timeout the journal is kept and `needs_user_action` is reported.
   Tests use the `#[cfg(test)] with_recovery_network_timeout` override with a connector that never answers.
5. svn-client's `journal::move_no_replace` is now `pub` and syncs the folders of both names. The app's copy is removed.

### #104 — case-insensitive file systems
- `local_util::target_in` reads the flag from `LocalEnv::case_insensitive`. The default is `CASE_INSENSITIVE_FS` (`cfg!(macos | windows)`); tests switch it with `RealEngine::with_case_insensitive_fs`. `target` keeps the constant for `commit.rs`.
- When wc.db has no row for the path as spelled, `case_variants` searches wc.db component by component, with NFC and full case folding.
  - Several matches are refused as ambiguous.
  - One match is used only when the disk confirms it (`spelling_on_disk`), as follows. (This follows review B1 on PR #115: case-sensitive APFS volumes and per-folder case-sensitive Windows directories exist.)

  | On disk | Result |
  |---|---|
  | Neither name exists | the wc.db spelling |
  | Both exist and are the same item: same dev/ino on Unix, and no folder on the way lists the asked spelling as an entry of its own | the wc.db spelling |
  | Only the asked name exists | taken as spelled |
  | Anything else (two different files, or only the versioned one) | refused (`WcInvalidOperation`) |

  As a result, a request never reverts, deletes or edits a different file than the one named.
- The move destination uses `new_target`, which keeps the exact spelling.
- `unversioned_items` drops items that match a versioned node up to case whenever `env.case_insensitive` is set.
- Tests run on Linux, which plays the case-sensitive volume:
  - helper level: missing items resolve; distinct `README.md`/`README.MD`, or only the versioned name present, are refused; a separate file next to a missing versioned one is taken as spelled; ambiguous matches are refused; aliases are dropped;
  - engine level, with the flag on: revert of `README.MD` with remove_unversioned is refused and `README.md` keeps its edits; cleanup with remove_unversioned keeps `README.MD`; revert of `readme.md` restores a missing `README.md`.

### #99 items 1 and 4
- `svn_protocol::segments_below_root` (new, public) wraps the WP0 rule (`decode_path` + `below_root`, ASCII-only root folding).
  `sync::same_location` now uses it.
  Tests cover a `Dự-Án` root in both crates: lowercase and unnecessary escapes match, ASCII root case matches, non-ASCII case does not.
- `WcRegistry::add_unfinished` runs `read_dir` and the canonicalize calls in `spawn_blocking`.

## Gates
See the PR body for counts.

## Notes / limits
- Journal owner hints hash `wcroot` exactly as WcDb names it (not canonicalized), like the existing `data.wcroot == db.wcroot()` comparison.
  The app always opens a registered WC through its stored root, so both spellings agree.
- svn oracle tests that need svn 1.15 skipped here: the container has svn 1.14.3 from apt.
