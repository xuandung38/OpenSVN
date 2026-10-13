# 10a: listing cap error code and streaming enforcement (approval 5507571395 follow-up)

Branch `feat/10a-http-ra-session`. Merged the #75 head (3333434) and `origin/main` (802822f) first.

## Changes
- `svn_types::ErrorCode::LimitExceeded` (new): a valid request whose result would exceed an
  engine size cap. Not corruption, not retryable.
- `opensvn-ipc`: `LimitExceeded` maps to `invalid-argument`, `retryable=false`, no actions (no
  Cleanup). No new IPC code, so the FE contract and `bindings.ts` are unchanged (gen:ipc output
  checked: no drift). Snapshot updated; doc 03 section 4 notes the use of `invalid-argument`.
- User message (engine message, shown as `AppError.message`): "the folder listing has more
  than N entries, which is more than OpenSVN lists at once; list a smaller folder or a lower
  depth" (and the same for more than N levels).
- `svn-ra` `list`: the entry cap is enforced inside the `stream_items` callback of `list_one`
  with a per-request budget (cap minus entries already listed or queued), so one huge
  directory is never buffered beyond the cap. It now applies to `Immediates` and `Files` too
  (`Files` counts children of every kind, since all are read before the filter). The level cap
  also uses `LimitExceeded`.

## Tests
- `list_infinity_enforces_the_limits`: now expects `LimitExceeded` for both caps.
- `one_level_listings_enforce_the_entry_limit`: Immediates and Files, exact cap passes, cap-1 fails.
- `entry_limit_is_enforced_while_streaming`: 5000 entries (~2 MB) followed by a broken tail.
  With cap 10 the error is `LimitExceeded`; without the cap it is `Corrupt`. That shows the cap
  fires before the body is fully read.
- `opensvn-ipc`: `limit_exceeded_is_final_and_offers_no_cleanup`, snapshot row added.

## Gates (CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0, clean target dir)
- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: pass
- `cargo clippy -p opensvn --features real-engine --all-targets --locked -- -D warnings`: pass
- `cargo test -p svn-ra -p svn-ra-api -p svn-types -p opensvn-ipc --locked`: 281 passed, 0 failed, 11 ignored
- `cargo test --workspace --locked --no-fail-fast`: 1186 passed, 0 failed, 20 ignored
- gen:ipc: no drift
- Not run: live compat_read against mod_dav_svn, and the FE lint, typecheck and test (no FE change)
