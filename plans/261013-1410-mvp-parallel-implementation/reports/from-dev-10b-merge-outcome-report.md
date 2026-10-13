# 10b RA write: MERGE outcome handling (review 5507088699)

## Blocking B1: close_edit no longer deletes after an unknown MERGE outcome
- `merge()` returns `CommitFailure::{NotMerged, OutcomeUnknown}`.
- NotMerged: the failure happened before the MERGE was sent, or the server answered the MERGE with a non-2xx status. `close_edit` deletes the transaction.
- OutcomeUnknown: a send error (cancel, transport), a cancelled or failed body read, or an unparseable 2xx answer. The transaction is kept, and `close_edit` returns the new `ErrorCode::CommitOutcomeUnknown`. The message names the transaction, and the child error is the cause.
- This matches libsvn_ra_serf, which never deletes after a MERGE was sent.
- IPC maps `CommitOutcomeUnknown` to a non-retryable `network` error, and the snapshot is updated. A dedicated IPC code can follow in the app PR.

## Non-blocking
- N1: new `HttpTransport::send_unattended` and `RaWire::send_unattended`. These never call the `AuthPrompter`: a 401 becomes `AuthRequired`, and a certificate that needs a decision becomes a TLS error. Cached credentials and accepted certificates are still used.
  - `merge::cleanup_txn` uses this and gives up after 15 s (`CLEANUP_TIMEOUT`).
  - Used by the Drop task, `discard_txn`, `abort_edit` and the failed-start cleanup.
  - `abort_txn` (explicit reconcile) stays interactive.
- N2: the `abort_edit` doc now says a dropped `abort_edit` future is not retried. The journal and `abort_txn` cover that case.

## Tests
- `ops_commit`:
  - `merge_cancelled_in_flight_keeps_the_transaction`: delayed MERGE, cancelled at 200 ms. Returns `CommitOutcomeUnknown` with a Cancelled child, sends no DELETE and makes no on_merged call.
  - `unreadable_merge_success_keeps_the_transaction`: 3 bodies.
  - `definite_merge_refusals_delete_the_transaction`: 409, 423 and 500.
- `transport_auth::unattended_send_never_prompts_and_uses_cached_credentials`
- `transport_tls::unattended_send_never_asks_about_a_certificate`

## Gates
All run with `CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0`.
- fmt: pass
- clippy `--workspace --all-targets -D warnings`: pass
- `cargo test -p svn-ra -p svn-ra-api -p svn-protocol -p svn-types -p opensvn-ipc`: 460 passed, 0 failed, 8 ignored
- `compat_write --ignored` against local Apache + mod_dav_svn 1.14.3 (scratchpad/dav75): 3 passed
- Not run: the Docker compat servers (no Docker daemon), and a test for a transport reset after MERGE (wiremock cannot inject it; the same code path as cancel).
