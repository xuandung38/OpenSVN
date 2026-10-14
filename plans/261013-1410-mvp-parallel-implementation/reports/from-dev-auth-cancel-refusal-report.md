# Auth: cancelled prompt no longer sticks (#108) and #107 review follow-ups: report

Branch `fix/auth-cancel-refusal`. It is based on `origin/fix/wp2-followups` (4ab2332, PR #107), because #107 had not been merged into `main` when this work started. **Dependency:** merge #107 first. After that, merge `origin/main` into this branch (merge commit) and re-run the gates.

## #108: a cancelled or timed-out prompt fails only the requests waiting for it

`crates/svn-ra/src/auth/mod.rs`
- `AuthManager` has a new transport-wide counter, `denials`. Each `Attached` records it as `seq` when the request is prepared. This covers `prepare`, the `on_unauthorized` retries, new Negotiate contexts, and `renew`.
- When a prompt round ends without credentials (cancel, broker timeout, a Negotiate answer that cannot be used, or the 3-attempt give-up), `deny()` sets `denied` and stamps `denied_seq`.
- In `on_unauthorized`, a request is refused because of a denial only if `denied_seq > sent.seq`, meaning the request was already under way when that round ended. Requests prepared later go on to the normal path:
  - an interactive request prompts again;
  - an unattended request still fails with `AuthRequired` and does not prompt (the existing "not prompting" path).
- The old generation comparison for requests that carried no credentials has been removed. That comparison caused the bug: the request saw generation 0 while the flight was at 1.
- Lockout protection is unchanged:
  - a cancel does not touch `failures`;
  - a rejection before a cancel still counts toward the cap;
  - the 3-attempt cap and the reset after a give-up behave as before.

## #107 review notes

- **(a) Confirmation only on a challenged retry.** `Attached::challenged` is set only on the Basic credentials that `on_unauthorized` returns, so the URL is known to be protected. `on_success` does two things only when the request was challenged: it resets `failures`, and it reports `credentials_accepted`. A preemptive header that gets a 2xx on an anonymous path no longer confirms a typed password. It also no longer resets the failure counter.
  - Changed test: `a_server_error_does_not_confirm_typed_credentials`. A later preemptive success on another URL no longer confirms the password. This is intended.
- **(b) Sign-out race.** `AuthBroker` keeps a sign-out counter per realm (`sign_outs`). `credentials()` records the counter at the moment the password is typed (`typed_at`). `credentials_accepted` saves the password only if no sign-out of that realm has happened since. It checks again after the keychain write, and deletes the secret if a sign-out landed while the write was running. A sign-out of another realm has no effect.
- **(c) Wording.** `stored_credentials_rejected` clears `awaiting`. Before this, a rejected saved password was worded as "the password you entered last time".
- **(d) Docs.**
  - Doc 03 §5: the stale duplicate `auth://prompt` paragraph is gone. Its `auth_respond` decision rules are merged into the current paragraph.
  - Doc 03 now also describes how a cancel or timeout behaves.
  - The doc 03 SQL comment now covers the rules for challenged-only confirmation and for sign-out.
  - The connector module doc now says "Unattended opens never queue behind a prompt". It also explains that interactive requests to the same origin and realm do wait on the single prompt, by design.

## Tests added

- `svn-ra/tests/transport_auth.rs`:
  - `a_cancelled_first_prompt_does_not_block_the_next_interactive_request`: the repro from #108. The second prompt is attempt 0 with reason `Initial`.
  - `a_cancelled_prompt_after_a_success_also_prompts_again`: the path where a `hosts` entry already exists.
  - `unattended_request_after_a_cancelled_prompt_fails_without_prompting`.
  - `requests_queued_behind_a_cancelled_prompt_all_fail_and_none_prompts`: the owner plus 3 queued requests all return `AuthRequired` within 5s, with one prompt in total. The next request then prompts.
  - `a_cancel_does_not_count_towards_the_three_attempt_cap`: the attempts are [0,1,1,2], and exactly 3 wrong passwords reach the server.
  - `an_anonymous_success_does_not_confirm_a_typed_password`.
- `engine/real/tests/connector_tests.rs`:
  - `a_timed_out_sign_in_prompt_does_not_block_the_next_attempt`: a real broker with a 300 ms timeout. Background work fails fast, and the retry prompts and signs in.
- `auth/tests.rs`:
  - `a_password_typed_before_a_sign_out_is_not_saved_by_a_late_response`;
  - `a_rejected_saved_password_is_not_worded_as_the_one_typed_last_time`.

Without the `svn-ra` fix, four of the new transport tests fail: the #108 repro, the queued-requests test, the anonymous-success test, and the updated 500 test. The connector timeout test fails too.

## Gates (own worktree and target dir, oracle variables set with `OPENSVN_REQUIRE_SVN=1`)

| Gate | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | pass |
| `cargo clippy -p opensvn --features real-engine --all-targets --locked -- -D warnings` | pass |
| `cargo test --workspace --locked` | pass: 1284 passed, 0 failed, 21 ignored |
| `cargo test -p opensvn --features opensvn/real-engine --locked` | pass: 410 passed, 0 failed |
| `gen:ipc` (`gen_ipc` binary) | no drift |
| FE gates | not run, because no frontend files changed |

## Remaining

- Merge `origin/main` into this branch once #107 lands, then re-run the gates.
- Known narrow case: a request prepared just before a cancel, whose 401 arrives just after it, fails with the cancel and does not prompt. That is intended, because it belongs to the cancelled round.
