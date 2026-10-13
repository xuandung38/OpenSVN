# P15 RealEngine (PR #72): review fixes

Branch `feat/p15-real-engine-m1`, origin/main merged (merge commit, no rebase).

## Items from the lead review

| # | Item | Fix | Tests |
|---|------|-----|-------|
| 1 | Compile on main (`RepoDisplayPath`) | `mapping.rs` uses `.to_string()` for changed path and copy-from | `mapping::tests::log_entries_carry_display_paths_as_text` |
| 2 | `recover_locks` deleted rows from the registry alone | Replaced by `db.set_lock_owner(LockOwner{store, pid, boot_id})` + `WcDb::wc_cleanup_locks()` (marker + levels + canonical root); remaining rows reported as foreign. New `RealEngineConfig::boot_id`. | `dead_owner_record_without_marker_keeps_the_row`, `a_dead_app_lock_with_its_marker_is_released_once`, `a_foreign_lock_is_reported_and_never_stolen`, `locks_are_recovered_whichever_spelling_of_the_root_is_opened` |
| 3 | Blame n*m LCS (OOM) | `svn-diff` with `max_cost` (`DISPLAY_MAX_COST`) and context 0; each rebuilt text capped at 5 MiB (`SharedBuf::with_limit`); both fail with "file is too large to annotate" | `a_revision_over_the_size_limit_is_refused`, `a_comparison_over_the_cost_budget_is_refused`, `a_large_ordinary_history_stays_within_the_budget`, `lines_keep_the_revision_that_introduced_them` |
| 4 | Prop-only revision wiped attribution | Sink wrapper records the closing `window(None)`; without it the text and attribution stay. Contract pinned in `FileRevHandler::revision` docs (svn-ra-api). | `a_revision_without_any_window_keeps_text_and_attribution`, `a_trailing_revision_without_any_window_keeps_the_text`, `an_explicitly_emptied_file_has_no_lines` |
| 5 | Lost txn blocked commits forever | `ensure_authenticated` before `abort_txn`; `PathNotFound` = lost: journal cleared + `needs_user_action[""]`; other errors keep the journal for the next open | `a_transaction_the_server_no_longer_knows_clears_the_journal`, `a_temporary_abort_failure_keeps_the_journal_for_the_next_open` |
| 6 | Finalize under foreign lock | Merged journal is not finalized while a foreign row or unfinished work items remain (reported, journal kept) | `a_merged_commit_is_not_finalized_while_a_foreign_lock_is_held`, `a_merged_commit_is_not_finalized_over_unfinished_work` |
| 7 | Unencoded branch URLs | `repos_root().join(RelPath)`; server names validated with `RelPath::from_server` (unsafe names skipped with a warning) | `branch_urls_are_percent_encoded_and_unsafe_names_skipped` |
| 8 | Journal/edits/locks moderate items | Unreadable journal moved to `<key>.json.corrupt` and reported; journal key and `wc_root` canonicalized; root-targeted remote edits refused; WC-scope lock/unlock hold `WC_LOCK` on the parent dir (WcBusy before any server call) | `an_unreadable_journal_does_not_block_opening`, `journals_are_keyed_by_the_canonical_root`, `the_repository_root_is_never_an_edit_target`, `deleting_the_repository_root_creates_no_revision`, `working_copy_locks_wait_for_a_directory_locked_by_another_program` |
| 9 | Conformance groups | Added `cancellation_and_errors` (checkout honors cancel before "not wired"); real `plant_lock` in the harness + recovery scenario on an svn-wc WC; skipped groups documented on `RealHarness` | `shared_suite_cancellation_and_errors`, `shared_recovery_scenario_with_planted_locks` |
| 10 | Shared helpers | `engine/text.rs` and `engine/mapping.rs` shared by fake and real engines | existing fake + real suites |

## Not done (follow-up)
- `svn:needs-lock` read-only/writable toggling on lock/unlock (needs the property layer; documented in `locks.rs`).
- Conformance groups needing checkout/status/commit stay skipped until svn-client is wired.

## Gates (local; disk on the shared box was nearly full, so scoped)
- `cargo fmt --all --check`: pass
- `cargo clippy -p opensvn -p svn-ra-api --all-targets -D warnings`, with and without `--features opensvn/real-engine`: pass (workspace-wide clippy not run: out of disk)
- `cargo test --locked -p opensvn --features opensvn/real-engine`: 183 passed, 0 failed
- `cargo test --locked -p opensvn` (feature off): 128 passed, 0 failed
- `cargo test --locked -p svn-ra-api --all-features`: 39 passed, 0 failed
- Not run: workspace-wide clippy/test, `cargo deny`, FE gates (no FE change), `pnpm tauri build`
