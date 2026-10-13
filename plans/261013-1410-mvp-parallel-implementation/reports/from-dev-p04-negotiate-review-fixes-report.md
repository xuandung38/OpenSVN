# P04 Negotiate: review fixes (PR #69)

Response to the REQUEST_CHANGES review of 0cfdd1f. origin/main merged first (clean).

## Changes
1. https only (blocking item). `try_negotiate` returns `NegotiateFailed` ("use an https:// URL")
   for any `http` challenge offering Negotiate, before the provider is called. The proactive
   branch of `prepare` requires `https` too. There is no http opt-in. Test:
   `plain_http_never_sends_a_negotiate_token` (wiremock http server: one request, no
   `Authorization`, `initiate` never called).
2. Transient retries (5xx, network error) never resend a Negotiate token. `attempt` calls
   `AuthManager::renew` for a fresh context first. If that fails, it sends without credentials and
   the challenge decides. Test: `transient_retry_sends_a_fresh_token_not_a_replay`.
3. Kerberos only, with or without a final token. `check_kerberos_token` parses the first token
   (raw krb5, or the first SPNEGO `mechTypes` entry). Both backends call it before returning a
   token, so an NTLM/NegoEx token is never sent. GSSAPI also checks that the completed context's
   mech OID is krb5. On SSPI, cross-krb5 already checks the package after a final token. It does
   not expose `QueryContextAttributes` on a pending context; this is documented. GSSAPI keeps
   SPNEGO instead of the raw krb5 mech because IIS expects SPNEGO, and libgssapi 0.11 has no
   `gss_set_neg_mechs`. This deviation from "request krb5 mech OID" is documented in the module
   and the checklist.
4. The SPN is pinned to the URL host. `service_host` strips a trailing dot, lowercases the host,
   and refuses IP literals with a `NegotiateFailed` "full host name" message before any provider
   call. GSSAPI uses `HTTP@host` with `GSS_C_NT_HOSTBASED_SERVICE`. The krb5.conf settings
   `dns_canonicalize_hostname` and `rdns` are documented in the checklist.
5. Failures are remembered for `NEGOTIATE_RETRY_AFTER` = 5 min. `HttpTransport::with_negotiate_retry_after`
   changes it. A rejected proactive token gets one fresh context through the normal challenge
   before the host counts as failed. For a Negotiate-only server, the error carries the last
   failure reason as a child error. Tests: `negotiate_failure_memory_expires`,
   `rejected_up_front_token_retries_one_fresh_context_before_prompting`.
6. TLS channel bindings are implemented. reqwest `tls_info(true)` gives the peer certificate of
   each response. `tls_server_end_point` hashes it per RFC 5929: SHA-256 for MD5, SHA-1 and
   SHA-256 signatures, SHA-384 or SHA-512 when the signature uses them, including RSA-PSS; there
   is no binding for Ed25519. The result goes to `cross-krb5` and `libgssapi`. Limitation
   (documented): the certificate comes from the latest response for the origin, not from the
   exact pooled connection. No GitHub issue was opened because the feature is implemented.
7. Rewrote the `ErrorCode::NegotiateFailed` doc and the IPC `NegotiateFailed` doc, regenerated
   `bindings.ts` (doc comment only), and updated doc 03 and the ADR-003 auth row.
8. The PR description now covers the `redact.rs` changes.

`NegotiateProvider::initiate` now takes `channel_bindings: Option<&[u8]>`.

## Gates (local, at the pushed head)
- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets -- -D warnings`: pass
- `cargo clippy -p svn-ra --all-targets --features negotiate-gssapi -- -D warnings`: pass
- `cargo clippy -p svn-ra --all-targets --target x86_64-pc-windows-msvc -- -D warnings`: pass.
  This is a type-check only: ring's C build ran with a no-op CC and a stub AR. Nothing was linked
  or run on Windows.
- `cargo test -p svn-types -p svn-ra -p opensvn-ipc -p svn-ra-api`: 192 passed, 0 failed, 1 ignored
- `cargo test -p svn-ra --features negotiate-gssapi`: 85 passed, 0 failed, 2 ignored
- Real Kerberos: `tests/negotiate_gssapi_kdc.rs` (ignored by default) passed against a throwaway
  MIT krb5 1.20.1 KDC. It covered the SPNEGO token check, acceptance with channel bindings, the
  mech OID check, mismatched bindings refused, and a forged AP-REP refused.
- FE: `pnpm lint` (biome) passes and `pnpm typecheck` passes. `vitest` had 144/144 passing with
  `--testTimeout=30000`. The default 5 s timeout failed in the remote/* tests at load average ~24
  from other agents. The only FE change is a doc comment in `bindings.ts`.

## Remaining
- Real VisualSVN/IIS verification on Windows, macOS and Linux (PO checklist), including Extended
  Protection set to Required and the first-token check against real SSPI output.
- Channel bindings are not tied to the exact pooled connection (documented).
