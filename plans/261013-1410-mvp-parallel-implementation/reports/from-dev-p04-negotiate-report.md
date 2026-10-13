# P04 M2: HTTP Negotiate/Kerberos (PR #69)

## Done
- `svn_ra::auth::negotiate`: `NegotiateProvider` / `NegotiateContext` traits, `Authorization:
  Negotiate <b64>` (sensitive header), server token parsing, `system_provider()`.
- Windows: SSPI `Negotiate` package via `cross-krb5 =0.5.0` (safe API, so no `unsafe` and no lint
  override). `NEGOTIATE_TOKEN | DISABLE_CONFIDENTIALITY`; mutual auth stays required; cross-krb5
  rejects a context that negotiated NTLM.
- macOS/Linux: `libgssapi =0.11.0`, SPNEGO mech, `HTTP@host`, `GSS_C_MUTUAL_FLAG` checked on the
  completed context. Behind the `svn-ra/negotiate-gssapi` feature (off by default). It needs gssapi
  headers and libclang to build.
- All provider calls (initiate, step) run in `spawn_blocking` wrapped in `or_cancel`.
- Flow: a 401 offering Negotiate starts a context and retries. A 2xx carrying `Negotiate <token>`
  must pass `step` (mutual auth), or the response is discarded with `NegotiateFailed`. With no final
  token the response is accepted (debug log). After a success the host gets Negotiate up front on
  later requests, which also lets one-shot bodies through. A failure (no ticket, rejected ticket,
  continuation leg, bad mutual token) is remembered per host. The user then gets the Basic prompt
  with `PromptReason::NegotiateFailed`, and Basic is only ever sent over https. A server that offers
  only Negotiate returns `ErrorCode::NegotiateFailed`. A rejected Negotiate token does not use up a
  Basic attempt.
- New `svn_types::ErrorCode::NegotiateFailed` is mapped to the IPC `negotiate-failed` code with
  the `sign-in` action. Updated: snapshot, bindings.ts, and doc 03.
- Docs: ADR-003 auth row, `docs/visualsvn-iwa-checklist.md` (build notes, flow, NTLM decision).

## Gates
- `cargo fmt --all --check`: pass.
- Scoped clippy `-p svn-types -p svn-ra -p opensvn-ipc -p svn-ra-api --all-targets -D warnings`:
  pass. Also passes for `svn-ra` with `--features negotiate-gssapi`.
- `cargo clippy --workspace --all-targets -- -D warnings`: pass.
- Tests `-p svn-types -p svn-ra -p opensvn-ipc -p svn-ra-api`: 173 passed, 0 failed, 1 ignored.
  `svn-ra --features negotiate-gssapi`: 66 passed, 0 failed, 1 ignored (including a real GSSAPI
  no-ticket smoke test).
- Windows: `cargo clippy -p svn-ra --all-targets --target x86_64-pc-windows-msvc -D warnings` passes.
  `ring`'s C build was stubbed with a no-op CC/AR because there are no MSVC headers here. This is
  type-check only; nothing was linked or run on Windows.
- `gen_ipc` regenerated `bindings.ts` with no diff.
- FE (apps/opensvn): biome lint, tsc typecheck and vitest all pass (21 files, 144 tests). An
  earlier run under heavy CPU load hit 5 s vitest timeouts in the remote/* tests; they pass on
  rerun.

## Remaining
- Real-server verification on Windows/macOS/Linux using the checklist (PO).
- If a transient-status retry (503) re-sends the same Negotiate token, a server replay cache may
  reject it. The host then falls back to the prompt for the session. This is rare and left as is.
- The desktop app does not depend on `svn-ra` yet. When it does, forward `negotiate-gssapi` for the
  macOS/Linux release builds.
