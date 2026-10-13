# 11C: fixes for the REQUEST_CHANGES review of #76

Branch `feat/11c-client-m1`. origin/main was merged first (merge commit, no conflicts). svn 1.15.0 (nix) was
used as the oracle with `OPENSVN_REQUIRE_SVN=1`.

## Blocking

- **B1, delete without force.** `local::delete` now runs svn's `find_undeletables` rules, but only when
  neither `force` nor `keep_local` is set (`--keep-local` checks nothing, like svn). It refuses with
  `WcInvalidOperation` plus `svn_code` 195006 for: an added file (even an empty one), an added directory
  with properties or files, a modified copy or copy child, a replacement by a plain add, a modified
  versioned file or directory, and a conflict. Unversioned items that are not ignored are refused with
  200005. Missing nodes, unmodified copies and empty added directories are deleted, as svn does.
  Directories are now removed recursively once the checks pass, which removes ignored items too (the
  non-blocking "ignored only" item). `DeleteOptions` gained `global_ignores` and is no longer `Copy`.
  Tests in `local_ops_vs_svn.rs`: `delete_never_removes_local_work_without_force` (10 cases, each one
  run on svn and on us: same code, and rows, status and disk unchanged and identical),
  `copied_file_is_modified_on_disk_after_copy`, `delete_allows_what_svn_allows`, and
  `delete_of_a_missing_added_file_is_allowed`.
- **B2, locks.** The harvest gathers every lock token the walk meets (`Harvest::locks`): those of
  committed paths, of unmodified locked files below the targets (subject to depth and the changelist
  filter, like svn), and every BASE lock at or below a deleted directory
  (`WcDb::repos_locks_below`). All of them are sent. The journal records them (`JournalData::locks`), and
  the finalize transaction drops their WC rows unless `keep_locks` is set. It deletes a row only while
  the row still holds the sent token (`WcTxn::lock_remove_token`), and before BASE is removed. A
  lock-only path keeps its BASE revision (verified with svn). The fake server now checks locks
  recursively on `delete_entry` and, like mod_dav_svn's MERGE, releases every lock whose token was sent.
  Tests in `commit_fake.rs`, each compared with svn on a twin checkout of the `file://` repo:
  `deleting_a_locked_tree_sends_and_releases_every_token`,
  `unmodified_locked_files_below_the_targets_are_released` (rows equal to svn's), and
  `locks_outside_a_changelist_filter_are_kept`.
- **B3, half a move.** `collect` rejects a moved-here root whose source is not committed, and a
  moved-away root whose destination is not committed. The error carries `svn_code` 200009 and svn's
  message, and is raised before any server contact. Test: `one_side_of_a_move_cannot_be_committed_alone`
  covers a file and a directory, both directions, with svn's E200009 checked on a twin.
- **B4, cancel after MERGE and stale journals.** After MERGE the cancel token is ignored. A finalize
  failure is reported as `WcNeedsCleanup` with "committed revision N, …", and the journal stays. After a
  successful finalize the journal is removed before the work queue runs, and the queue runs with a fresh
  token. A queue failure becomes `CommitResult::wc_warning`, not an error. `finalize` skips any item
  whose BASE revision is already newer than the journal's revision. `commit` finalizes this WC's merged
  journals before it harvests. Tests: `cancel_after_merge_still_finishes_the_commit` (new RecRa hook
  `cancel_after_merge`), `stale_journal_never_moves_base_backwards`, and
  `next_commit_finalizes_a_pending_merged_journal_first` (the last two in `commit_failpoints.rs`).

## Non-blocking

- Root BASE revision: `drive` opens a changed session root at its BASE revision. The fake now checks a
  directory's out-of-dateness when its properties change (in PROPPATCH style), no longer when it is
  opened. That matches mod_dav_svn and allows mixed-revision commits. `fake_commit_and_reads.rs` was
  updated for this. Test: `root_property_change_on_an_outdated_root_is_out_of_date`, with svn's own
  failure checked first.
- Change inside an uncommitted copy: refused before contacting the server, with svn's wording. Test:
  `change_inside_an_uncommitted_copy_is_refused`.
- Directories that hold only ignored items are deleted like svn (see B1).
- A WC-to-WC copy of a directory copies unversioned items (ignored ones included), which stay unversioned.
  This was checked against svn 1.15 first. Test: `copy_dir_takes_unversioned_items_along`.
- `resolve`: properties, the conflict record and the marker removals are written in one transaction
  (`WcTxn::write_conflict` / `clear_conflict`).
- WQ path safety: already in place. Every `FileRemove`/`FileTranslate`/`FileMove` operand goes through
  `resolve_operand` → `safe_join` (`svn-wc/src/workqueue/paths.rs`), which is covered by
  `items_cannot_escape_the_working_copy`. Nothing was added.

## Non-vacuity

Each fix was reverted on its own and its tests rerun. All 11 mutations failed their tests: B1 schedule
check, keep-local, ignored, B2 gather, B2 recursive, B3, B4 cancel, B4 skip-newer, copy-inside, root
revision, copy unversioned.

## Gates (separate target dir, `CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_INCREMENTAL=0`)

- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets -- -D warnings`: pass. It ran before the final one-file test
  edit in svn-ra-api; `clippy -p svn-ra-api --all-targets --all-features` was rerun afterwards and passes.
- `cargo clippy -p svn-wc -p svn-client --all-targets --features failpoints -- -D warnings`: pass
- `cargo test -p svn-wc -p svn-ra-api -p svn-client --all-features`: 35 suites, 392 passed, 0 failed.
  Of those: commit_failpoints 9, commit_fake 26, commit_vs_svn 2, local_ops_vs_svn 24.

## Not done

- The `#[ignore]` live test against local Apache was not written. The `OPENSVN_COMPAT_LOCAL_*` harness
  and the HTTP commit editor only exist on `feat/10b-ra-write-m1` (PR #75), not on main or this
  branch, so there is no real-server path for svn-client to drive yet. It should be added once #75 lands.
