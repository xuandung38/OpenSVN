# Wave 0 contract PR review: #24 (01a contracts) + #23 (01b app skeleton/CI)

Date: 2026-10-13 · Reviewer: code-reviewer · Mode: read-only (scratch copies only, nothing pushed)

## Scope / state reviewed
- **#24** `feat/01a-contracts` @ `1aad379`. **NOTE: #24 was MERGED to `main` at 08:15Z during this review** (merge `edc36d1`). 01a findings below therefore need a follow-up `contract:` PR, ideally before wave-1 packages (10b, 07, 11) start coding against `EditorWorker` / `safe_join`.
- **#23** `feat/01b-app-skeleton-ci` @ `7fb8ea6` (rebased on main, adds `refactor: use shared engine types, workspace dependencies and workspace lints`). Earlier head `ea973e4` also checked.
- Checks run (rustc 1.98.1, scratch copies):
  - 01a: fmt OK, clippy `-D warnings --all-features` OK, `cargo test --workspace --locked` OK, `cargo deny check` OK.
  - 01b @7fb8ea6: fmt OK, clippy OK, `cargo deny check` OK (with the 2 new ignores), `cargo test` flaky (see M10).
  - Live CI run 38368686583: all three Windows jobs **FAIL**: web (CRLF, C3), tauri build (icon.ico, C4), rust (install-action flake; C3 and C4 would follow). macOS and the Linux tauri/web/bindings/deny jobs pass. rust (ubuntu) was still pending.
- Extra repro tests (scratch only): EditorWorker hang, RelPath bypasses, APFS case folding.

## Findings

### CRITICAL
**[CRITICAL] C1. EditorWorker: `close_edit`/`abort_edit` hang forever while any `WindowSender` is alive**
- Evidence: `crates/svn-ra-api/src/editor_worker.rs:146`. The worker loop ends only when *every* `Sender` is dropped, and `WindowSender` holds a `tx` clone (`:448-452`). After `CloseEdit`/`AbortEdit` the loop only sets `finished = true` (`:159`) and keeps blocking in `blocking_recv`. `shutdown()` (`:499-503`) drops the handle's own `tx`, then awaits `join`, which never completes.
- Repro (scratch test): open_root → add_file → `apply_textdelta` (keep the sender) → `abort_edit()`. It timed out after 3 s, so the hang is reproduced.
- Impact: rule 1 explicitly allows several files to be open at once with late `close_file`, so the 10b driver will keep `WindowSender`s in download tasks. On cancel or error, `abort_edit` deadlocks: one blocking thread leaks, the WC lock is never released, and the "cancel < 1 s" requirement (red-team #6) is broken. The fake driver hides this because it sends all windows inline and drops the sender (`fake/update_drive.rs:143-150`).
- Recommendation: once a close/abort (or a poisoning failure) has been processed, call `rx.close()`, drain with `try_recv()` (calling `reject` on each command), then `break`. Later `WindowSender::send` calls then fail fast with the stored error. Add a regression test that keeps a live `WindowSender` across `abort_edit` and `close_edit`.

**[CRITICAL] C2. `.svn` admin-dir check bypassed by Unicode case folding and NTFS 8.3 aliases**
- Evidence: `crates/svn-types/src/rel_path.rs:77` and `crates/svn-types/src/safe_join.rs:26` both use `eq_ignore_ascii_case(".svn")`.
  - `RelPath::from_server(".\u{017F}vn/wc.db")` (U+017F LATIN SMALL LETTER LONG S) is accepted. I confirmed on this Mac (APFS, case-insensitive default) that `.ſvn/` resolves to an existing `.svn/` directory.
  - `from_server("SVN~1/wc.db")` is accepted. On NTFS with 8.3 names enabled, that is the short name of `.svn` (the same class as git CVE-2014-9390 / `GIT~1`).
- Impact: a malicious repository can write into `.svn/wc.db` or the pristine store. This breaks the MVP security acceptance criterion and the DoD rule that every server path goes through `from_server` + `safe_join`.
- Recommendation: compare a full Unicode case fold, not ASCII. For example, `seg.to_lowercase()` after mapping `ſ`→`s`, or simpler: reject when `seg.chars().flat_map(char::to_uppercase).collect::<String>() == ".SVN"`. Strip the HFS+ ignorable code points (U+200C–U+200F, U+202A–U+202E, U+206A–U+206F, U+FEFF) before comparing, as git's `is_hfs_dotgit` does. Reject `svn~<digits>` case-insensitively, as git's `is_ntfs_dotgit` does. Put this in one shared helper used by both `from_server` and `safe_join`, and add tests.

**[CRITICAL] C3. (#23) CI is red on Windows: CRLF checkout, no `.gitattributes`**
- Evidence: CI run 38368686583, job "web (windows-2025)" failed. `biome check .` reports format diffs ending in `␍` for `apps/opensvn/package.json`, `capabilities/default.json`, `tauri.conf.json`, `App.tsx`, `biome.json`, `package.json` and others. There is no `.gitattributes` in the repo.
- The same root cause will also fail the Rust test `committed_bindings_are_up_to_date` on Windows (`apps/opensvn/src-tauri/src/lib.rs:72-74`): it compares `include_str!` of a CRLF-checked-out `bindings.ts` against specta's LF output. The rust windows job was still pending; this is predicted from the same cause.
- Recommendation: add `.gitattributes` with `* text=auto eol=lf` (plus `*.png binary`). It is a new root file, so the lead needs to assign an owner (suggest 01b).

**[CRITICAL] C4. (#23) Windows build fails: `icons/icon.ico` is missing**
- Evidence: CI run 38368686583, job "tauri build (windows-2025)": `tauri-build failed: ...\src-tauri\icons/icon.ico not found; required for generating a Windows Resource file`. Only `icons/icon.png` is committed, and `tauri.conf.json:33` lists only that file.
- The same build script runs during `cargo clippy` and `nextest` in the rust windows job, so that job will fail too. Its current failure is an unrelated `install-action` bash-startup flake (job 115199961797), which a re-run will not fix.
- Recommendation: add an `icon.ico` placeholder (e.g. generate it with `pnpm tauri icon icons/icon.png`) and list it in `bundle.icon`. `icons/` belongs to P14, so the lead should approve the placeholder in 01b.

### IMPORTANT
**[IMPORTANT] I1. `safe_join` relies on RelPath invariants that `RelPath::new` does not guarantee (Windows root replacement / traversal)**
- Evidence:
  - `rel_path.rs:30-45`: `check_structure` rejects only empty/`.`/`..` segments and NUL. I verified `RelPath::new("..\\..\\evil")`, `RelPath::new("C:\\Windows")` and `RelPath::new("a/C:")` are all `Ok`.
  - `safe_join.rs:32` calls `out.push(seg)` with no per-segment check. On Windows, `PathBuf::push` of a segment with a prefix (`C:`) replaces the root, and `\..\` traverses.
  - The spec says the "result must lie under root", but this is never asserted.
- Impact: `RelPath::new` will be fed `wc.db` `local_relpath` values (WCs are on-disk data and can be copied or zipped, so they are not trusted) and webview-supplied paths (P08). `safe_join` is the trust boundary, but it only holds for `from_server` input.
- Recommendation: reject `\` and `:` in `check_structure` for every constructor (they are never valid in svn relpaths). In `safe_join`, assert that each segment gives exactly one `Component::Normal` and that `out.starts_with(root)` at the end.

**[IMPORTANT] I2. `CommitObserver::on_txn_created` cannot fail, so the commit journal cannot veto a commit**
- Evidence: `crates/svn-ra-api/src/commit.rs:41-46` returns `()`. Phase 11 (line 27) and phase 13 crash recovery depend on the txn name being stored durably *before* any content is sent.
- Impact: if the journal write fails (disk full, permissions), the RA keeps uploading. A crash then leaves a txn that nothing recorded, so it can never be cleaned up.
- Recommendation: change it to `fn on_txn_created(&self, txn: &str) -> Result<()>`. Document that on `Err` the RA must DELETE the txn and fail `commit_editor` (or the first edit call). Do the same for `on_merged`, or document that it must not fail.

**[IMPORTANT] I3. The commit path has no cancellation**
- Evidence: `session.rs:189-196` `commit_editor(..)` takes no `CancelToken`, and none of the `CommitEditor` methods (`commit.rs:51-111`) take one either. `apply_text` uploads whole files.
- Impact: cancelling a large commit or a Browse upload (`remote_edit`) can only work by dropping the future, which skips the DELETE-txn cleanup. Red-team #6 (cancel < 1 s) is not met for commit.
- Recommendation: add `cancel: &CancelToken` to `commit_editor` (the editor keeps a clone, and `apply_text`/`close_edit` honor it, aborting the txn on cancel). Alternatively, add it per call to `apply_text` and `close_edit`.

**[IMPORTANT] I4. (#23) The plaintext password in the IPC type has a derived `Debug`**
- Evidence: `crates/opensvn-ipc/src/commands.rs:141-151`: `AuthResponse::Credentials { password: String, .. }` with `#[derive(Debug, Clone, PartialEq, ..)]`.
- Impact: any `tracing::debug!(?resp)` (or a future command-args log in P08) writes the password to the logs or the diagnostics export (red-team #13). The engine side (`Credentials` uses `SecretString`) is correct; the IPC side is not.
- Recommendation: write a manual `Debug` that redacts `password`, or use a `#[serde(transparent)]` newtype with a redacting `Debug` (plus `#[specta(type = String)]`). Convert to `SecretString` immediately in P08.

**[IMPORTANT] I5. `RepoUrl::parse` accepts embedded credentials**
- Evidence: `crates/svn-types/src/ids.rs:36-55`: only scheme, host, query and fragment are checked, so `https://user:pass@host/repo` is accepted. `Display` (`:78-82`) prints it in full.
- Impact: credentials can leak into logs, `AppError.detail`, the IPC `url` fields and the stored WC `repos_root`. It also bypasses the per-origin credential store.
- Recommendation: reject a URL that has a username or password (error code `Internal`/validation). Credentials go only through `AuthPrompter`.

### MODERATE
- **[MODERATE] M1. CancelToken waker list never shrinks.** Evidence: `cancel.rs:69-76`. `Cancelled` has no `Drop`, so wakers registered by futures that completed or were dropped stay until `cancel()`. Each poll scans the list (O(n)). A long-lived (session- or app-scope) token polled from many tasks grows without bound. I checked lost-wakeup correctness: the flag is set before taking the lock, and poll re-checks under the lock, so there is no race. Recommendation: store slab keys and remove them on `Drop`, or document "one token per operation".
- **[MODERATE] M2. The EditorWorker panic path never calls `abort_edit`.** Evidence: `editor_worker.rs:165-168`. The editor is then dropped. Rule 4 ("WC left restartable") can then only be met by the editor's `Drop` and its journal. Recommendation: say this in the `DeltaEditor` rule 4 doc ("editors must mark incomplete before mutating; `Drop` without `abort_edit` must be safe"), so 07/11 design for it.
- **[MODERATE] M3. RecordingEditor does not check path/parent consistency.** Evidence: `recording_editor.rs:184-192, 199-230`. `add_file("x/y/z", root)` is accepted, and so is `set_target_revision` after `open_root`. 10b driver path bugs would pass the fake. Recommendation: assert `path.parent() == dirs[parent].path`, and that `set_target_revision` comes before `open_root`.
- **[MODERATE] M4. LockOwnershipStore semantics.** Evidence: `lock_ownership.rs:64-69, 51-56`.
  - `record` silently overwrites an existing live owner.
  - The key is a non-canonical `wcroot` `PathBuf`, so case or symlink variants on macOS/Windows make `forget` miss.
  - `for_current_process` treats every other live process (svn CLI, a second app instance) as dead, so cleanup would break their locks if this is used outside tests.
  - Recommendation: document that `wcroot` must be canonical; make `record` return `WcBusy` when a different alive owner exists; mark the helper as test-only in its name or docs.
- **[MODERATE] M5. `SvnError::from_io` never produces `FileInUse`.** Evidence: `error.rs:98-108`. Windows `ERROR_SHARING_VIOLATION` (32) and `ERROR_LOCK_VIOLATION` (33), and `ErrorKind::ResourceBusy`, all become `Internal`, so the IPC Retry action for `file-in-use` never fires. Recommendation: map `raw_os_error()` on Windows.
- **[MODERATE] M6. `write_file_replacing` on Windows.** Evidence: `safe_join.rs:83`. `fs::rename` onto a read-only destination fails (`svn:needs-lock` files are read-only), and antivirus/indexer handles cause transient sharing violations. 07/11 will hit both. Recommendation: clear the read-only attribute before the rename, and retry with backoff on sharing violations (or document that callers must).
- **[MODERATE] M7. Server-derived paths are untyped `String`.** `LockInfo.path`, `ChangedPath.path`, `FileRev.path`, `DirEntry.name`, and `copy_from: Option<(&str, Revnum)>` (`editor.rs:127,147`, `model.rs:62,79,113,119`, `session.rs:75`). This matches the spec, but the DoD rule "every server path through `from_server`" is not enforced by the types. This is a lead decision; at minimum add doc comments saying "validate with `RelPath::from_server` before touching disk".
- **[MODERATE] M8. Windows-invalid characters and `COM0`/`LPT0` are accepted** by `from_server` (verified with `a<b>|?*"` and `COM0`). This is not an escape, but checkout on Windows fails mid-drive with `Internal` instead of an early `UnsafePath`. Recommendation: reject `<>"|?*` and `COM0`/`LPT0` (Microsoft lists them as reserved).
- **[MODERATE] M9. CommitEditor `apply_text(&mut self)` is sequential**, while phase 10 says "PUT in parallel, bounded". Recommendation: the contract doc must state whether `apply_text` may return before the upload finishes (with errors surfacing at `close_file`/`close_edit`). Otherwise 10b and 11C will implement different semantics.
- **[MODERATE] M10. (#23) The bindings tests race under `cargo test`.** Evidence: `apps/opensvn/src-tauri/src/lib.rs:33-41`. Both tests share `temp_dir()/opensvn-bindings-{pid}`, and one test's `remove_dir_all` deletes the other's file. Reproduced: 2 of 3 local runs failed. CI is only green because nextest runs each test in its own process. Recommendation: use a unique `tempfile::tempdir()` per call.
- **[MODERATE] M11. (#23) The capability is broader than the spec.** Evidence: `capabilities/default.json:7` `"core:default"` grants window, webview, menu, tray, image, resources, path, event and app defaults. The spec says "only the app's commands". Recommendation: narrow it to the core permissions the shell actually uses (likely `core:event:default` and `core:window:default`). The rest of the security config is good: CSP strict (no `unsafe-inline` or remote origins in prod), `freezePrototype`, `withGlobalTauri: false`, `main` window only, and a test asserting perms == commands + 1.
- **[MODERATE] M12. (#23) CI hygiene.**
  - No `timeout-minutes` on any job (spec: CI < 15 min).
  - The drift check covers only `bindings.ts`, not `permissions/autogenerated/` or `gen/schemas`.
  - `libssl-dev` is installed although OpenSSL is banned.
  - Caret ranges in `apps/opensvn/package.json` devDeps (`^16.3.0`, `^27.0.0`, `^19.2.0`) contradict "pin exactly".
  - Permissions (`contents: read`, `pull_request` not `_target`) are OK. All 6 action SHAs were verified against their tags (annotated tags dereferenced).
- **[MODERATE] M13. (#23) Ownership and contract change.** The PR edits 01a-owned files: the root `Cargo.toml` (adds tauri, tauri-build, tauri-specta, specta, specta-typescript, serde_json, `time`; members gets `apps/opensvn/src-tauri`) and `deny.toml` (ignores RUSTSEC-2024-0436 `paste` via specta and RUSTSEC-2024-0370 `proc-macro-error` via gtk3-macros, both proc-macro only, justified). The PR body still says the tauri/specta deps are pinned in the app crate, not the workspace, which is now stale. `time` is a new dependency outside the ADR-003 list. This needs explicit lead sign-off as a contract change.

## Integration #24 ↔ #23 (verified)
- Workspace members: `["crates/*", "apps/opensvn/src-tauri"]` in #23. OK.
- ErrorCode: at `ea973e4` the local shim had 22 variants, identical in name and order to `svn_types::ErrorCode`. At `7fb8ea6` the shim is replaced by `svn_types::ErrorCode` with an exhaustive `match` (`opensvn-ipc/src/error.rs:120-146`), so the mapping is exact by construction. `Corrupt`→`wc-needs-cleanup`, `UnsafePath`→`internal` plus a log line, as specified.
- Timestamp: `micros_to_iso` uses i128 nanos with `checked_mul` and RFC 3339 output. Tested for epoch, fractional and overflow. `timestamp_to_iso` silently falls back to 1970 on out-of-range values (minor: prefer `Option` or a log line).
- Cargo.lock: only additions. No pre-existing package version changed (diffed name/version sets).
- cargo-deny with the full tauri tree: licenses, bans and sources pass; no openssl/native-tls in the tree.

## Not blocking / verified OK
- CancelToken lost-wakeup ordering is correct (M1 is a leak, not a race).
- The `svn-types` no-runtime test works. The `.svn`/ADS/trailing-dot/reserved-name rules and NFC normalization behave as documented, apart from C2 and M8.
- `AcceptAlways` limited to `UnknownIssuer`/`SelfSigned`; `Credentials.password` is a `SecretString`.

## Merge recommendation
- **#24 (01a)**: already merged. Open a follow-up `contract:` PR now with **C1, C2, I1** (blocking for 07/10b/11), plus **I2, I3, I5** (they change signatures, so do it before 10b/11C start), and M1–M9 as the lead decides. Until C1 is fixed, 10b must not hold a `WindowSender` across `close_edit`/`abort_edit`.
- **#23 (01b)**: **merge after fixes**: C3 (`.gitattributes`) and C4 (`icon.ico`), with all 3 OS green, I4 (redact password), M10 (test temp dir), plus the lead's sign-off on M13 (Cargo.toml/deny.toml/`time`) and a decision on M11. M12 can follow.

## Unresolved questions
1. Does the NTFS `$UpCase` table fold U+017F to `S` (so C2 also applies on Windows)? Verified on APFS only. Either way the fix is the same.
2. Should webview-supplied paths use a separate constructor (e.g. `RelPath::from_client`) with `from_server` strictness? This affects P08.
3. Who owns `.gitattributes` (a new root file not in the ownership table)?

Status: DONE
