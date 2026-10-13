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
- `cargo test -p opensvn` (+ `-p opensvn-ipc`): see the PR comment for the counts
- `pnpm lint` pass, `pnpm typecheck` pass, `pnpm test`: see the PR comment
- `pnpm gen:ipc`: no drift

## Not done / caveats
- Windows and macOS code is unverified (no runner). `secure-store` Linux Secret Service has not been run against a real daemon.
- `cargo deny` and `pnpm tauri build` were not run this round (disk space).
- The updater has no endpoint configured in `tauri.conf.json` (no release repo or public key yet). Until it has one, update commands return `invalid-argument`.
