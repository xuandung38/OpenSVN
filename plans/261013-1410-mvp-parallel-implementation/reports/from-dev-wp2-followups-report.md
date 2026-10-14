# WP2 follow-ups (review of #103): report

Branch `fix/wp2-followups`, from `origin/main` at cbe88ab (WP2 #103 merged).

## What changed, per review item

1. **Typed passwords are saved only after the server accepts them.** New `AuthPrompter::credentials_accepted(origin, credentials)` in `svn-ra-api` (it does nothing by default). The transport calls it once per prompt answer, on the first response to a request carrying that answer that is neither a 401 nor a 5xx. It is never called for stored credentials, and never for an answer the server rejected (a rejection clears the pending answer). `AuthBroker::credentials` no longer writes anything. `credentials_accepted` writes the keychain secret first, then the account row (which clears `stale`), and deletes the old secret if the user name changed. The broker remembers typed answers the server rejected. The next first prompt for that origin, even from a rebuilt transport, carries `reason: rejected` with `attempt: 0`, and the UI shows "rejected last time, so it was not saved".
2. **The stale flag is persisted.** Migration 2 adds `account.stale INTEGER NOT NULL DEFAULT 0`. `stored_credentials_rejected` marks stale the row of the user name that was actually offered, and also keeps the in-memory set as a fallback. `stored_credentials` never offers a stale row. As a result, background work fails with `AuthRequired` without sending the stale password, across restarts and transport rebuilds. The password stays in the keychain. The next interactive prompt carries `reason: expired`, which the UI now explains. Only a newly accepted saved password clears the flag (`account_upsert`).
3. **Connector cache.** Matching is on whole segments, and unit tests cover `/svn/repo` against `/svn/repo2`, the longest root winning, and ASCII-only folding (KELVIN SIGN). A scripted-server test covers two sibling repositories. Case-differing URLs (VisualSVN) hit the cache when the repository is known to ignore case. That is known once an open at a differently cased URL was answered with the server's spelling, and it is recorded per root (`folds_case`). Folding is never assumed, because on a case-sensitive server `/svn/Repo` and `/svn/repo` can be different repositories. So when the first open used the exact spelling, one extra OPTIONS teaches the cache.
4. **No waiting behind a prompt.** The connector's open gate is now keyed per requested URL instead of per origin, and gates nobody holds are pruned. In the transport, an unattended 401 handler no longer queues behind a credentials prompt of the same flight: it fails with `AuthRequired` as soon as a prompt is waiting. This is a `watch` flag with a drop guard. Interactive requests still single-flight as before.
5. **Sign-out.** Secrets are deleted first, and the loop continues past errors. A row is deleted only when its secret is gone. A row whose secret could not be deleted is kept (so there is no orphaned secret, and signing out again retries) and is marked stale, so the secret is not offered any more. Failures are reported together in an `AppError` whose detail lists them. The transport is always invalidated.
6. **Removing a pin that exists only in memory.** `AppDbTrustStore::forget` returns whether it removed an in-memory pin, and `forget_certificate` invalidates when either the row or the memory pin was removed.
7. **Doc 03.** `network.negotiate` added to §6.3. Also documented in doc 03: `account.stale`, save after acceptance, the sign-out order, and the meaning of `expired` and of `rejected` at attempt 0.

Frontend: `CredentialReason` gains `expired`. The sign-in dialog has new messages for `expired` and for `rejected` at attempt 0, in English and Vietnamese. `bindings.ts` is unchanged, because `AuthPromptReason::Expired` already existed.

## Tests added

- `svn-ra/tests/transport_auth.rs`:
  - reported as accepted only after success, and once;
  - rejected then cancelled: never reported;
  - a 500 does not confirm;
  - stored credentials are never reported;
  - an unattended request does not wait behind a prompt.
- `auth/tests.rs`:
  - stale flag persisted across a new broker, with prompt reason `Expired`, the secret kept, and the flag cleared by a new accepted save;
  - a typed password that was rejected is never saved, and the next prompt says why;
  - sign-out with a store whose delete fails for one host;
  - forgetting a pin that exists only in memory rebuilds the transport.
- `connector_tests.rs` (scripted HTTPS server, which now has optional `second_repo` and `ignore_case`):
  - sibling repositories;
  - VisualSVN spellings hit the cache;
  - no folding until learned;
  - a waiting prompt does not block background work on another repository (or on another path of the same one);
  - a typed password is saved only after acceptance, including across a rebuild and a restart;
  - a stale saved password stays withheld after a restart.
- Connector unit tests for `find_root` and `under_folded`. Migration test: version 1 rows survive and start as not stale.

## Gates (worktree, own target dir)

- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets -- -D warnings`: pass
- `cargo clippy -p opensvn --features real-engine --all-targets -- -D warnings`: pass
- `cargo test --workspace --locked`: pass, 1276 passed, 0 failed
- `cargo test -p opensvn --features opensvn/real-engine`: pass, 407 passed, 0 failed
- `pnpm gen:ipc`: no drift
- FE: install, biome lint, typecheck, lint:safety, vitest (29 files, 210 tests), build + check-dist: all pass

## Remaining / notes

- The `expired` prompt does not pre-fill the user name: `AuthPrompt` has no user name field, and adding one would change the IPC contract.
- Seen in passing and not changed here (behaviour of the existing transport): after the user cancels a prompt, a later request with no credentials for the same origin on the same transport seems to fail with `auth-required` without prompting. That lasts until the transport is rebuilt, because `on_unauthorized` compares the request's default generation 0 with the flight's generation. One connector test works around it with a fresh connector. It should be confirmed with a test and fixed separately.
