# 11C: merge of main after #74 landed

Branch `feat/11c-client-m1` (PR #76). The merge commit b9c1f32 brings in origin/main at 3f69dad,
which includes #74 (feat/11u-client-m1), #73 and the rest of main.

## Merge
- No conflicts. `Cargo.lock` and `crates/svn-wc/src/db/txn.rs` merged automatically.
- The txn.rs hunk is #74's F1 change to `mark_base_deleted_at`: no overwrite of working rows at
  or below the op_depth. It sits next to this branch's `commit_node`, `lock_add`, `lock_remove`
  and `set_props` and does not overlap them.
- The other changes from main are #74's F1 update code (`update/{editor,file_install,finish}.rs`)
  and its oracle tests, plus FE and IPC files from #73.
- No fixes were needed after the merge.
- PR #76 no longer depends on an unmerged PR. Its diff against main is now only the 11C work.

## Gates (svn 1.15.0 nix on PATH, `OPENSVN_REQUIRE_SVN=1`, own target dir)
- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets -- -D warnings`: pass. This was the full workspace,
  Tauri app included.
- `cargo clippy -p svn-wc -p svn-client --all-targets --features failpoints -- -D warnings`: pass
- `cargo test -p svn-wc -p svn-ra-api -p svn-client --all-features`: 0 failed.
  - svn-client: 103 passed, including `commit_vs_svn` 2, `local_ops_vs_svn` 19,
    `oracle_vs_svn` 24 and `commit_failpoints` 7.
  - svn-ra-api: 39 passed.
  - svn-wc: 228 passed plus 1 ignored (the benchmark).

## Not run
- `cargo deny` is not installed.
- FE gates were not run: this merge changes no FE code on this branch.
- The shadowed-update regression tests from `test/11u-f1-regressions` (PR #82) are not on main
  yet, so they did not run here.
