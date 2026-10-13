# 11U: merge main + review follow-up F1 (PR #74)

Branch `feat/11u-client-m1`.

## Merge of origin/main (#71 svn-wc status, #72 RealEngine)
- Merge commit; the only textual conflict was `Cargo.lock` (svn-wc deps: `ignore` from main,
  `jiff` from this branch; both kept).
- svn-wc: main's `status/` module and this branch's `db/{bindings,txn}` live side by side; the
  new `PristineSource::text_size` from main has no implementor on this branch. svn-client keeps
  its own `status` module (wiring it to `svn_wc::status` is a separate cleanup).

## F1: shadowed update under a local delete or replace
svn 1.15 moves BASE on below a locally deleted or replaced node (`fb->shadowed` /
`db->shadowed`); we used to record the tree conflict and leave BASE at the old revision, which
re-raised the conflict on every update and left the replacement out of date for commit.
- File replaced locally + incoming edit: BASE row gets the new revision, checksum (pristine
  installed) and props; the working row, ACTUAL and the disk are untouched; tree conflict
  `replaced/edited`.
- Directory deleted or replaced locally + incoming change below: the drive continues with a
  `shadow` op_depth (the working layer's). BASE rows of the directory and its subtree move on,
  directory props go to BASE only, server adds below get BASE rows plus `base-deleted` rows,
  server deletes below only remove BASE. The tree conflict is recorded once, on the directory.
- `WcTxn::mark_base_deleted_at` now follows svn's `db_extend_parent_delete`: it never writes
  over an existing working row at or below that op_depth (a child of a directory replaced by a
  *copy* keeps its copied row when the server adds the same path; a plain re-add sits deeper and
  never reaches the rule; corrected after review, pinned in `test/11u-f1-regressions`).

Oracle tests (each: compare after update, `svn resolve --accept working`, compare, unrelated r3,
second update of both, compare, assert no conflict and BASE at HEAD, then the real svn commits the
identical working copy without an out-of-date error):
- `incoming_edit_of_locally_replaced_file_vs_svn`
- `incoming_edit_under_locally_deleted_dir_vs_svn` (edit, file add, subdir add, dir and file
  prop changes)
- `incoming_edit_under_locally_replaced_dir_vs_svn` (the replacement re-adds a file the server
  edits and one the server also adds)
The first two fail without the fix ("NODES rows differ").

## Gates (nix svn 1.15.0 on PATH, `OPENSVN_REQUIRE_SVN=1`)
- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets -- -D warnings`: pass (full workspace)
- `cargo clippy -p svn-wc -p svn-client --all-targets --features failpoints -- -D warnings`: pass
- `cargo test -p svn-client -p svn-wc -p svn-ra-api`: svn-client 49, svn-wc 225 (+1 ignored
  bench), svn-ra-api 39; 0 failed
- `cargo test -p svn-client -p svn-wc --features failpoints`: svn-client 50, svn-wc 227; 0 failed

## Still open
- F2 `--set-depth` (crop/expand).
- F3 non-UTF-8 names in status.
- Commit-ability is checked through the real svn on the identical working copy; our own commit
  path does not exist yet.
