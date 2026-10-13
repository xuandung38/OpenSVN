# 11U F1 follow-up: shadowed-update regression tests

Branch `test/11u-f1-regressions` from main at 3f69dad (#74 merged). Addresses the two
non-blocking items of the last #74 review (on e3fff94) and turns the reviewer's probes P1 to P8
into oracle tests in `crates/svn-client/tests/oracle_vs_svn.rs`.

## Tests added (svn 1.15.0 oracle: disk, NODES, ACTUAL, `svn status`)
Every test except the P8 one then runs `resolve_then_update_again`: resolve, unrelated r3,
second update, no conflict, BASE at HEAD, and real svn commits the same working copy.

| Probe | Test |
|---|---|
| P1 | `incoming_file_delete_under_locally_deleted_dir_vs_svn` |
| P2 | `incoming_subdir_delete_under_locally_deleted_dir_vs_svn` |
| P5 | `incoming_subdir_replaced_by_file_under_locally_deleted_dir_vs_svn` |
| P3 | `incoming_delete_of_readded_file_under_replaced_dir_vs_svn` |
| P4 | `incoming_delete_of_readded_subdir_under_replaced_dir_vs_svn` |
| P6 | `incoming_edit_of_modified_file_under_replaced_dir_vs_svn` |
| P7 (item 1) | `incoming_add_into_copy_replaced_dir_keeps_copied_rows_vs_svn` |
| P8 (item 2) | `incoming_delete_in_copy_replaced_dir_keeps_modified_file_unlike_svn` |

### Item 1: the `mark_base_deleted_at` guard
P7 replaces `docs` by a copy of `src`, so the copied `docs/lib` rows sit at op_depth 1, the
shadow's op_depth. The server adds `docs/lib`. The test asserts full parity with svn and that
the copied `op_depth=1` rows of `docs/lib` and `docs/lib/deep.rs` survive.

Revert check: with `WcTxn::mark_base_deleted_at` changed back to
`INSERT OR REPLACE ... VALUES (...)`, this test fails with "NODES rows differ", and it is the
only one of the 20 `incoming_*` tests that fails. Restored afterwards.

The doc comment on `mark_base_deleted_at` now names the copy case. The F1 report's wrong reason
("a re-added child in a replaced directory") is corrected too.

### Item 2: deliberate divergence (kept, pinned)
Setup: `src` is replaced by a copy, the user edits `src/lib/deep.rs`, and the server deletes
that file. svn 1.15 deletes the user's modified file from disk, so it then shows as missing. We
keep the file: below a shadowed directory, `delete_entry` only removes BASE and never touches
the disk.

What the test asserts:
- NODES and ACTUAL rows equal svn's.
- svn's disk lacks the file, and ours still has the user's text.
- The rest of the disk is identical.
- Our `svn status` shows the file as `M`.

If svn ever stops deleting the file, the test fails with a message that says the divergence may
be gone. There is a code comment at `update/editor.rs` `delete_entry` saying this is deliberate
and must not be "fixed".

## Gates (svn 1.15.0 nix on PATH, `OPENSVN_REQUIRE_SVN=1`)
- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets -- -D warnings`: pass. This was the full workspace,
  Tauri app included.
- `cargo clippy -p svn-wc -p svn-client --all-targets --features failpoints -- -D warnings`: pass
- `cargo test -p svn-client -p svn-wc`: svn-client 57 passed, svn-wc 225 passed plus 1 ignored
  (the benchmark), 0 failed
- `cargo test -p svn-client -p svn-wc --features failpoints`: svn-client 58 passed, svn-wc 227
  passed plus 1 ignored, 0 failed

## Remaining
- No production behaviour changed: only tests and comments were added.
- F2 (`--set-depth`) and F3 (non-UTF-8 names in status) are still open.
