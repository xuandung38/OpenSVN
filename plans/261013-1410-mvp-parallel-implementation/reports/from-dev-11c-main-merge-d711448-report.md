# 11C: merge of main at d711448

Branch `feat/11c-client-m1` (PR #76, approved at 9319144). The merge commit 3bde6a4 brings in
origin/main at d711448, which includes #62, #69, #80, #82 and #86.

## Merge
- No conflicts. No file was changed on both sides since the merge base.
- Main's changes are in `apps/opensvn/src-tauri/src/engine/real/` and its tests,
  `crates/svn-ra/tests/`, and reports. This branch's changes are in svn-wc and svn-client.
- No fixes were needed after the merge. No features were added.

## Gates (svn 1.15.0 nix on PATH, `OPENSVN_REQUIRE_SVN=1`, own target dir)
- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets -- -D warnings`: pass
- `cargo clippy -p opensvn --features real-engine --all-targets -- -D warnings`: pass
- `cargo clippy -p svn-wc -p svn-client --features failpoints --all-targets -- -D warnings`: pass
- `cargo test --workspace --locked --no-fail-fast`: 1098 passed, 0 failed, 10 ignored
  (111 suites)
- `cargo test -p svn-wc -p svn-client --all-features`: 353 passed, 0 failed, 1 ignored
  (29 suites)

## Not run
- FE gates: this merge changes no FE code on this branch.
- `cargo deny` is not installed.
