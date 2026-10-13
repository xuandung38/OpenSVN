# WP0 follow-ups (review of #91) — dev report

Branch `fix/wp0-followups`, from `origin/main` 793c6f6. It covers the five non-blocking items
from the #91 review.

## Changes

1. **Backslash dot segments** (`svn-protocol/src/caps/options.rs`). `stub_path` now splits the
   raw header on both `/` and `\` before it refuses `.`/`..` segments. For http(s), the URL
   parser reads `\` as `/`, so `/svn/repo\..` was collapsing to `/svn`. Added the evil fixture
   `options/root-backslash-dotdot` (expect `reject`) through `generate.py`. Regenerating is
   byte-stable: two runs give no diff. The fixture fails on the old code ("hostile OPTIONS
   answer was accepted").
2. **ASCII-only case fold** (`svn-protocol/src/caps/url_builder.rs`). `below_root` now uses
   `eq_ignore_ascii_case`, so KELVIN SIGN (U+212A) no longer matches `k`. New test
   `repository_root_case_fold_is_ascii_only` checks three cases:
   - `k` versus `K` is still accepted;
   - `k` versus U+212A is refused, in both directions;
   - `é` versus `É` is refused.
3. **`AuthPrompter::stored_credentials_rejected(&CredentialRequest)`** (`svn-ra-api`). It is
   async and a no-op by default.
   - `AuthManager` records that the cached credentials came from the store (`from_stored`). When
     the server answers 401 to them, in either mode, it calls the hook once per offer, before it
     prompts (interactive) or fails with `AuthRequired` (unattended). The request carries
     `PromptReason::Rejected`.
   - The `stored_credentials` doc now states the contract: stored credentials are offered once
     per origin per transport, so background work must reuse one long-lived transport.
   - **403 is deliberately not reported.** mod_dav_svn answers 403 to valid credentials that
     lack path access, and a 403 is not a failed logon for the directory server. Reporting it
     would make the app drop a good password for no lockout benefit. This is documented on the
     hook.
   - WP2 (`feat/wp2-real-connector-auth`) implements the hook in `AuthBroker`. The app here
     still uses the default.
4. **New tests:**
   - `transport_http.rs::stored_credentials_are_never_offered_over_plain_http`: interactive and
     unattended, `stored_calls == 0`, no `Authorization` header sent, no hook call.
   - `transport_auth.rs::stale_stored_credentials_are_sent_once_per_transport_when_unattended`:
     - 5 unattended sends produce exactly 1 credentialed request, 1 store lookup, 0 prompts and
       1 hook call.
     - A second transport offers the password again. This documents why the transport must be
       reused and why the app must act on the hook.
   - `transport_auth.rs::stored_attempt_counts_towards_the_three_attempt_cap`:
     - stored, then two wrong prompts, gives exactly 3 credentialed requests, then
       `AuthRequired`;
     - the hook fires once (only for the stored attempt);
     - a later unattended send adds 0 requests.
   - Existing stored-credentials tests now also assert the hook: it fires once before the
     `Rejected` prompt, and not at all for accepted credentials.
5. **`fake/prefixed.rs::rel_of`** now compares decoded segments, refuses `%2F` (`decode`
   refuses a decoded `/`), and refuses a query or fragment, like `UrlBuilder::locate`. New test
   `session_urls_are_compared_decoded_and_encoded_slashes_are_refused`.

## Gates (own target dir, `CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0`)

- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: pass
- `cargo clippy -p opensvn --features real-engine --all-targets --locked -- -D warnings`: pass
- `cargo test --workspace --locked`: 1233 passed, 0 failed, 21 ignored (5 more than at #91)
- `cargo test -p opensvn --features opensvn/real-engine --locked`: 317 passed, 0 failed
- Regression checks: with the `svn-protocol/src` changes reverted, the new `caps_and_urls`
  tests and the `evil_server` options case fail.
- `fixtures/evil/generate.py` regenerates byte-identically (no diff after a second run).

## Not covered / remaining

- The app's `AuthBroker` still uses the default no-op hook. WP2 implements it.
- `compat_read` against a real mod_dav_svn was not re-run. This change does not touch the wire
  paths it exercises, apart from the stricter root checks.

## Round 2: review of #96 (REQUEST_CHANGES)

- **B1. Refuse, don't normalise.** New `svn_protocol::check_server_url_text`, in
  `caps/url_builder.rs`, refuses any server-supplied root, stub, href or `Location` that contains
  `\`, any control character (tab, CR and LF are what the WHATWG parser deletes) or a percent
  escape of a control character or of `\` (`%00`–`%1F`, `%7F`, `%5C`). The raw-path helper
  `raw_path_of` ends the authority at `/` or `\`.
  - `check_server_path_text` adds the dot-segment scan on top of that. Three places use it:
    - `options.rs::stub_path`;
    - `UrlBuilder::new`, for every stub. `Capabilities` has public fields, so the stubs can
      bypass `from_options_response`; the session URL's own path is exempt;
    - `accept_href`, which also covers propfind, update and `repo_path_of_url`. In listings
      these entries are still quarantined.
  - Redirect `Location` handling uses it too: `svn-ra` `redirect_target` and `svn-protocol`
    `moved_error`.
  - Tests:
    - the reviewer's four values, plus CR, LF, NUL, DEL, `%09`, `%0d`, `%0A`, `%00`, `%7F`,
      `%5C`/`%5c`, and `%5C` in the host;
    - every stub header;
    - `UrlBuilder::new` given unsafe stubs directly;
    - ordinary escapes such as `%20`, `%E2%82%AC` and `%2F` are still accepted;
    - new hostile hrefs;
    - `moved_error`;
    - a same-origin redirect test (`redirects_with_parser_rewritten_locations_are_refused`).
  - New evil fixtures: `options/root-abs-backslash-dotdot`, `root-tab-dotdot`,
    `root-encoded-tab-dotdot`, `root-encoded-backslash`, `me-tab` (76 cases). Rerunning
    `generate.py` gives no diff.
  - With `options.rs` reverted, the new `caps_and_urls` tests fail. `evil_server` still passes
    then, because the `UrlBuilder::new` re-check rejects the values.
- **N1.** The `stored_credentials_rejected` doc now covers the Apache 2.4 case: a user with a
  valid password who fails a `Require` rule also gets 401, unless the server sets
  `AuthzSendForbiddenOnFailure On`. The app must therefore mark the password stale (stop
  auto-offering it, keep it stored, re-prompt), not delete it.
- **N3.** The hook is no longer raced against `cancel`. A pending cancel takes effect right after
  the hook returns. If the future is dropped mid-hook, a drop guard (`RestoreFromStored`) sets
  `from_stored` again. The new test `cancel_during_the_rejection_hook_keeps_the_report` covers
  interactive and unattended mode; it fails with `or_cancel` put back.

### Gates, round 2 (after merging origin/main c482176; own target dir, `CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0`)

- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: pass
- `cargo clippy -p opensvn --features real-engine --all-targets --locked -- -D warnings`: pass
- `cargo test --workspace --locked`: 1251 passed, 0 failed, 21 ignored
- `cargo test -p opensvn --features opensvn/real-engine --locked`: 338 passed, 0 failed
- `python3 fixtures/evil/generate.py`: 76 cases, no diff
