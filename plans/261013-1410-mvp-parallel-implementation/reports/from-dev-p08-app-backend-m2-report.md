# P08 app backend M2 (PR #62): follow-up round

Branch `feat/p08-app-backend-m2`. origin/main merged (merge commit, no rebase).

## Merge
- Conflicts: `Cargo.lock` (taken from main, re-resolved; the app's own pins such as `tauri-plugin-updater 2.13.3`, `notify 8.2.0` and `zip 4.6.1` keep their versions), the `CertDecision` doc comment, and `bindings.ts` (regenerated, no drift).
- Main dropped `PromptReason::Expired`, so the broker mapping for it was removed.
- FE: the `copy_location` call from 12a now passes `fromWc: null` (the contract of this PR added that field).

## Items
| Item | Status | Where / tests |
|---|---|---|
| N1 Linux start time since boot | DONE | `lock_ownership/alive_probe.rs`: the stamp is `starttime` (field 22 of `/proc/<pid>/stat`, ticks since boot). `btime` is never added, because a clock step moves it. If both boot ids are exact (Linux `boot_id`, macOS session UUID) and they differ, the owner is Dead before any pid check (`rebooted_since`). Tests: `linux_clock_step_keeps_a_live_owner_alive_because_ticks_ignore_btime`, `after_a_reboot_with_exact_boot_ids_...`, `linux_ticks_agree_with_wall_clock_start_minus_btime` (cross-checks against sysinfo with `getconf CLK_TCK`), `only_two_different_exact_boot_ids_prove_a_reboot` |
| N2 biased select | DONE | `wc/actor.rs` was already biased and uses CAS. Also changed: the FakeEngine `network_tick` (cancel first), the poller (settings change and shutdown first) and the status watch loop (events before the debounce deadline). Tests: `a_cancel_that_coincides_with_the_end_of_a_step_always_wins` (paused clock, 64 rounds) and the source guard `every_select_in_the_app_is_biased` |
| N3 bidi in displayed strings | DONE | New `display_text.rs`, used everywhere. It **rejects**: program args and path (confirm dialog), and `path_pick` title and suggestedName (also refused: `/`, `\`, `.`, `..`, length). It **escapes** (`\u{XXXX}`): grant `displayName`, notification title and body, WC name. The set now also covers U+2028/2029, U+061C, U+180E, U+00AD, isolates, invisible operators and Hangul fillers. Tests in `display_text`, `path_grants`, `notify` |
| Updater | DONE | The token is stored only in the OS credential store (`secure-store` `KeyringStore`, pseudo-origin `opensvn-app://app:0`, realm `\u{1}app-secrets`). There is no fallback: `UnavailableSecretStore` refuses `set`. `MemorySecretStore` is `#[cfg(test)]`. Every `plugins.updater.endpoints` entry is validated before the builder exists and before the token header is added (https, host exactly `api.github.com`, default port, no userinfo, `/repos/<release repo>/`, no dot segments or `%2f`/`%2e`/`%5c`). The download URL is checked the same way. Without updater config, commands return `invalid-argument` instead of touching the unregistered plugin state. Tests: `endpoints_must_be_...`, `keychain_store_...`, `without_an_os_store_...`, `the_app_wires_the_os_store_...` |
| ReplacePin IPC mapping | DONE | `auth/broker.rs::map_cert_decision`: maps 1:1 and then checks with `CertDecision::is_valid_for`; anything invalid becomes `Reject`. `ReplacePin` on `Changed` now reaches the transport as `ReplacePin` (before, it was `AcceptOnce`, which the transport refuses). `AcceptOnce` on a hard failure is now `Reject`. Test: `ipc_cert_decisions_map_to_transport_decisions_the_transport_accepts` (every decision × failure), and the existing cases were updated |
| RedactingMakeWriter on all sinks | DONE | New `logging.rs`: stderr plus `<data>/logs/opensvn.log` (dir 0700, file 0600, rotated at 5 MB), both through `redacted()` = `svn_ra::transport::RedactingMakeWriter`. Installed first in `build_state`. Tests: `every_sink_receives_redacted_records` (userinfo, bearer, cookie), `log_file_is_private_appended_and_rotated`, and the source guard `no_sink_is_built_without_redaction` (no other `with_writer`/`set_global_default` in `src/`) |

Doc 03 §3.5: the certificate decision rules, `path_pick` validation, and the updater commands.

## Gates (local, Linux)
- `cargo fmt --all --check`: pass
- `cargo clippy -p opensvn -p opensvn-ipc -p secure-store --all-targets -- -D warnings`: pass. This is scoped: the workspace run was not done because the disk is shared and near full.
- `cargo test -p opensvn -p opensvn-ipc --locked`: 206 app tests and 26 ipc tests pass, 0 fail
- `pnpm lint` passes and `pnpm typecheck` passes. `pnpm test` (vitest, default 5 s timeout): between 1 and 3 of 144 tests time out in `features/remote/*`, and which ones changes from run to run. Load average was about 27 on 4 cores because other agents were building at the same time. With `--testTimeout=30000`, all 144 tests in 21 files pass. Each failing file also passes when run on its own. One real failure (the branch test expected no `fromWc`) was fixed.
- `pnpm gen:ipc`: no drift

## Not done / caveats
- Windows and macOS code is unverified (no runner). `secure-store` Linux Secret Service has not been run against a real daemon.
- `cargo deny` and `pnpm tauri build` were not run this round (disk space).
- The updater has no endpoint configured in `tauri.conf.json` (no release repo or public key yet). Until it has one, update commands return `invalid-argument`.

---

# Round 3: REQUEST_CHANGES on PR #62 (review of 10ff86e)

origin/main merged (#71, #72) with a merge commit. Conflicts: `Cargo.lock` (main's, then re-resolved), the app `Cargo.toml` (both dependency blocks kept), `engine/fake/{mod,remote_ops}.rs` (main moved `fake/text.rs` to `engine/text.rs`; `tick_tests` and the `Sched` import kept).

## Blocking: default-app opener (`os_integration.rs`)
| Item | Status | Where / tests |
|---|---|---|
| Trailing dot or space (Windows) | DONE | `resolve_in_wc` (reveal and open) now parses with `RelPath::from_server` rules: no trailing `.`/space, no reserved device names, no `:`. `default_app_allowed` also trims trailing dots and spaces itself before reading the extension. Test: `trailing_dots_and_spaces_are_refused_before_any_extension_check` |
| Symlinks | DONE | For `OpenWith::Default`, `symlink_metadata` on every component below the WC root; any link (or a component that cannot be inspected) is refused. Test: `symlinks_are_never_opened_with_the_default_app` (`README.txt -> Evil.app`, `notes.txt -> x.terminal`, a linked folder) |
| Bundles | DONE | A folder opens only if its name has no `.` (so `Evil.app`, `X.prefPane`, also `v1.2`, are revealed instead). Test: `folders_that_may_be_bundles_are_refused` |
| Allow-list | DONE | `DEFAULT_APP_EXTENSIONS`: txt md markdown rst log csv tsv json yaml yml toml diff patch, png jpg jpeg gif bmp webp tif tiff, pdf rtf doc docx xls xlsx ppt pptx odt ods odp (case-insensitive). No archives, disk images, html/svg, macro-enabled office files, or unknown/extensionless names. Execute bit still refuses on Unix. Test: `default_app_uses_an_allow_list_not_a_block_list` (all launchers from the review) |
| FE fallback | DONE | A refusal carries the new `ErrorAction::RevealInFileManager` (`"reveal-in-file-manager"`). `src/lib/open-in-os.ts::openWithDefaultApp` reveals instead and returns the reason (no UI calls `openExternal` yet, so this is the helper the UI will use). Tests: `src/lib/open-in-os.test.ts` |

## Non-blocking
| Item | Status | Where / tests |
|---|---|---|
| Updater `dangerous*` flags | DONE | `build.rs` fails the build when any `tauri*.conf.json` sets `plugins.updater.dangerous*` to anything but `false`; `configured_endpoints` refuses the same at runtime. Tests: `dangerous_tls_options_disable_updates`, `shipped_config_sets_no_dangerous_updater_option` |
| Redirect policy, sensitive header | DONE | `configure_client`: `https_only(true)` and a custom redirect policy (`redirect_allowed`: https, default port, no userinfo, hosts `api.github.com`, `objects.githubusercontent.com`, `release-assets.githubusercontent.com`, at most 5 hops). Headers built by `request_headers` with `set_sensitive(true)`. `reqwest` (workspace pin 0.13.5, the plugin's version) added to the app. Tests: `redirects_stay_on_https_github_hosts`, `authorization_header_is_sensitive_and_absent_without_a_token` |
| `install()` re-reads token | DONE | `update_install` already read the token from the keychain; `install` now replaces the `Update`'s headers with ones built from that token, so a token cleared after the check is not sent |
| Tag characters | DONE | `is_deceptive` covers U+E0000..U+E007F and U+1BCA0..U+1BCA3 |
| Rotated log in diagnostics | DONE | `opensvn.log.1` is included (still through `finish()` redaction). Test: `old_logs_and_huge_logs_are_trimmed` |
| Lock rows note | DONE | "Upgrade notes" in `docs/deployment-guide.md` and the `lock_ownership` module doc |
| `pnpm test -- --testTimeout` | DONE | pnpm forwards a literal `--`, and vitest ignores everything after it. `apps/opensvn/scripts/vitest.mjs` drops `--` markers and runs `vitest run`; the `test` script uses it. Verified: `pnpm test -- --testTimeout=30000 src/lib/` runs only `src/lib` |
| `installFakeClock()` | NOT AVAILABLE | Not on main; it exists only on `feat/12b-fe-wc-m1`. Not ported here, so FE timeouts under load are reported as they are below |

## Gates (local, Linux, load average about 20 on 4 cores)
- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets -- -D warnings`: pass (full workspace this time)
- `cargo test -p opensvn -p opensvn-ipc`: app lib 215/215, ipc 4 + 5 + 14 + 3, all pass
- `pnpm gen:ipc`: regenerated for the new `ErrorAction` variant; the committed bindings match
- `pnpm lint`, `pnpm typecheck`: pass
- `pnpm test` (default timeouts): 146/147. The failure was `history.test.tsx` "shows revisions, hostile text stays inert..." (2.2 s, a `waitFor` deadline under load). This file is not touched by this PR, and it passes on its own (8/8, twice).
- `pnpm test -- --testTimeout=30000` (the flag now reaches vitest): 147/147 in 22 files

## Not done / caveats
- Windows and macOS behaviour is still unverified: trailing-dot handling, `open` on bundles and the explorer launch. The symlink tests run on Unix only.
- The FE helper is not wired to a button yet, because no UI calls `openExternal` in this PR.
- `installFakeClock()` is not on main (it is on `feat/12b-fe-wc-m1`). The timing-sensitive FE tests still depend on machine load.

## Round 3 (merge of main after #73/#74, last blocking item)
| Item | Status | Where / tests |
|---|---|---|
| Merge origin/main | DONE | Merge commit. Conflicts in `Cargo.toml`, `app_state.rs`, `lib.rs`, `app_tests/{fixture,security}.rs` resolved by keeping both sides: main's `ProgramLauncher` (`launcher`) beside this PR's opener/notifier/menu/updates/secrets; `svn-diff` is now a non-optional dependency as on main. The duplicate `data_dir` field (added on both sides) was folded into one. `commands_settings.rs` got the new `launcher` dep |
| Allow-list (blocking) | DONE | `doc`, `xls`, `ppt` (legacy binary office, can carry macros), `odt`, `ods`, `odp` (Basic macros) and `rtf` (embedded OLE objects) removed from `DEFAULT_APP_EXTENSIONS`; all seven added to the refused names in `default_app_uses_an_allow_list_not_a_block_list`. Remaining office types: `docx`, `xlsx`, `pptx` |
| TOCTOU note | DONE | One-line residual-risk note in the `os_integration` module doc |
| Windows launcher | TODO left | `ShellExecuteW` would need a new direct `windows-sys` dependency (`Win32_UI_Shell`) that cannot be built or run here, so `explorer <path>` stays with a `TODO(windows)` that states the comma-splitting caveat (also on `reveal`'s `/select,`) |
| History/blame tests | Untouched | The merge did not touch them; migration to `installFakeClock()` is left for the separate PR |

Gates (Linux): fmt pass; clippy `--workspace --all-targets -D warnings` pass; `cargo test -p opensvn -p opensvn-ipc` all pass (app lib 227/227, ipc 4 + 5 + 15 + 3); `pnpm gen:ipc` no drift; `pnpm lint`, `lint:safety`, `typecheck` pass; `pnpm test` 183/183 in 25 files.
