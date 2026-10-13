# 10b: MERGE gateway answers and cleanup tests (approval follow-ups 1 and 5)

Branch `feat/10b-ra-write-m1`, after merging `origin/main` (d711448).

## MERGE answers that prove nothing (non-blocking 1)
`merge::refusal_is_definite(status, has_dav_error)` decides whether a non-2xx MERGE answer
lets `close_edit` delete the transaction:
- 408, 502, 504: never definite (proxy timeout or broken upstream; the backend may still be
  committing). Result: `CommitOutcomeUnknown`, transaction kept, the server's answer as child.
- other 5xx: definite only with a mod_dav_svn `D:error` body (e.g. hook failure); a bare 5xx
  keeps the transaction.
- 4xx (except 408): definite, with or without a `D:error` body. HTTP 4xx means the request was
  refused before it was carried out (authz, auth, proxy limits), so nothing can be merged.
  Treating a bare 403 as "unknown" would hide the real Forbidden error behind a scarier one.

## Tests
- `ops_commit::definite_merge_refusals_delete_the_transaction`: 409/423/500/503 with `D:error`,
  bare 403/409/413; each deletes and keeps the server's code.
- `ops_commit::gateway_and_bare_server_errors_after_merge_keep_the_transaction`: 502/504 (HTML,
  empty, 504 even with `D:error`), 408, bare/HTML 500, bare 503.
- `merge::tests::which_merge_refusals_are_definite` (unit).
- New `tests/commit_cleanup.rs` (HTTPS server whose DELETE needs Basic login):
  - `dropped_editor_cleanup_never_prompts` (plus control: interactive `abort_txn` prompts);
  - `abort_edit_and_failed_close_edit_never_prompt`;
  - `cleanup_gives_up_after_its_timeout`: paused tokio clock, DELETE delayed 1 h, gives up at
    15 s virtual time with `Timeout`.
  - Negative checks: with the cleanup made interactive, both prompt tests fail; with the timeout
    raised, the timeout test fails (gave up at 60 s, the read timeout).
- `tokio` dev-dependency of svn-ra gains `test-util`.

## Gates (CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0)
- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets -- -D warnings`: pass
- `cargo clippy -p opensvn --features real-engine --all-targets -- -D warnings`: pass
- `cargo test -p svn-ra -p svn-ra-api -p svn-protocol -p svn-types -p opensvn-ipc --no-fail-fast`:
  465 passed, 0 failed, 8 ignored
- Not run: Docker / live mod_dav_svn compat tests.

## Re-merge of origin/main 802822f (#76)
Merged cleanly. Gates rerun:
- fmt, workspace clippy, real-engine clippy (`--locked`): pass
- `cargo test --workspace --locked --no-fail-fast`: 1155 passed, 0 failed, 13 ignored
