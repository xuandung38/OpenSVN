# 12b PR #73: merge main (#71, #72) and reviewer follow-ups

Date: 2026-10-13 · Branch `feat/12b-fe-wc-m1` · Worktree `/home/user/wt-12b`

## Merge of `origin/main` (merge commit)
- Conflicts: `Cargo.toml`/`Cargo.lock` (svn-diff), `engine/fake/remote_ops.rs` (imports, `map_log_entry` now `mapping::log_entry` from #72).
- `svn-diff` is a regular dependency now (the display budget is used by both engines); it is no longer in the `real-engine` feature list.
- The FileDiff builder moved from FakeEngine into the shared `engine/text.rs` as `file_diff` (with its tests). `RealEngine::diff_rev` uses it, so it sets `tooComplex` with the same `svn_diff::DISPLAY_MAX_COST` budget and honours `DiffOptions::full_text`. New real-engine test: `revision_diff_over_the_display_budget_is_flagged_too_complex`. `diff_wc` on the real engine is still `not_wired` (unchanged from #72).
- `diff_external` works with `real-engine` for revision targets. Staging writes the text side bytes: non-binary means valid UTF-8 and the side is built from those bytes unchanged, so the copy is byte-exact. Files in other encodings are reported as binary and refused; a `TODO(p15)` in `open_external_diff` marks staging raw bytes for them.

## Reviewer follow-ups
1. `remove_tree` never follows symbolic links: it checks the folder with `symlink_metadata`, clears read-only only under `cfg(windows)` and only on regular files (walk via `symlink_metadata`), then `remove_dir_all` (removes links, does not descend). The sweep and size walk skip links too. Test: `removal_never_follows_planted_symlinks` (file link, folder link, link in the root; target keeps `0600` and its contents).
2. + 3. Copies are staged as `<id>/left.<ext>` and `<id>/right.<ext>`; `ext` is kept only if it matches `[A-Za-z0-9]{1,16}`, otherwise it is dropped (`left` / `right`). Nothing else from the repository reaches the file name. Tests pin `C:\x\a.txt`, `\\srv\share\a.txt`, `a.txt::$DATA`, `NUL .txt`, `COM1 .txt`, `CONIN$`, cmd metacharacters, bidi and over-long extensions.
4. Caps: at most 32 staged folders and 512 MiB across them; the oldest folders are removed first, and a single launch larger than the byte cap is refused. Staging is serialized by a mutex so the caps hold under concurrent launches. Tests for both caps.
5. Encoding: see above (byte-exact for UTF-8; TODO for other encodings).
6. `apps/opensvn/package.json` has a `lint` script (`biome check .`).
- Doc 03 updated for the new staging layout and caps.

## Gates (local)
| Gate | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | pass |
| `cargo clippy -p opensvn --all-targets --features real-engine --locked -- -D warnings` | pass |
| `cargo test -p opensvn -p opensvn-ipc` | pass, 167 (140 + 27), 0 fail |
| `cargo test -p opensvn --features real-engine` | pass, 196, 0 fail |
| `pnpm gen:ipc` + `git diff --exit-code bindings.ts` | no drift |
| `pnpm -C apps/opensvn lint` / root `biome check .` | pass |
| `pnpm -C apps/opensvn typecheck` | pass |
| `pnpm -C apps/opensvn lint:safety` | pass |
| `pnpm -C apps/opensvn test` | 180/180, 24 files |
| `pnpm -C apps/opensvn build` + check-dist | pass |

Not run: Windows/macOS builds (the `cfg(windows)` read-only reset is not compiled here).
