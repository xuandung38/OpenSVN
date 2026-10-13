# VisualSVN Server IWA (Negotiate) checklist

Manual verification of OpenSVN Negotiate (SSO) against a real VisualSVN Server.

## How it is built
- Windows: always on. SSPI `Negotiate` package through the `cross-krb5` crate (safe wrapper, no
  `unsafe` in OpenSVN). Only Kerberos is accepted; a context that falls back to NTLM fails.
- macOS/Linux: system GSSAPI with the SPNEGO mechanism through `libgssapi`, behind the cargo feature
  `svn-ra/negotiate-gssapi` (off by default). Building it needs the gssapi headers (`libkrb5-dev` on
  Debian/Ubuntu, `krb5-devel` on Fedora; the macOS SDK has them) and libclang for bindgen. At run
  time it needs `libgssapi_krb5` (the deb already recommends `libgssapi-krb5-2`). Without the feature
  the client treats a Negotiate-only server as `negotiate-failed` and offers Basic when the server
  also offers it.
- Flow: 401 with `Negotiate` → token for `HTTP/<host>` (built off the async runtime) → retry. When
  the server's 2xx carries `WWW-Authenticate: Negotiate <token>`, the token must pass mutual
  authentication or the response is discarded with `negotiate-failed`; a server that sends no
  final token is accepted. After the first success each request starts Negotiate up front (tokens
  are single use). Any failure is remembered per host for the session and the user gets the
  sign-in prompt with reason `negotiate-failed` (Basic, https only); a server without Basic gives
  the error `negotiate-failed`.

## NTLM decision
NTLM is not in the MVP. It needs several legs on one pinned connection, which the HTTP client does
not offer, and it is weaker than Kerberos. Short-name or IP URLs (no SPN match) therefore end in
the `negotiate-failed` prompt; use the server's full host name.

## Server side
- [ ] Server uses Windows authentication (IWA) and is reachable over https with a CA the client trusts.
- [ ] Note which providers the server offers: `curl -sI https://<host>/svn/<repo>` should show
      `WWW-Authenticate: Negotiate` and possibly `NTLM`; record the exact headers.
- [ ] SPN `HTTP/<fqdn>` is registered for the server's service account (`setspn -Q HTTP/<fqdn>`).

## Windows (domain-joined)
- [ ] Open the repository by FQDN: expect SSO with no prompt.
- [ ] Open by short name or by IP: Kerberos has no SPN match and SSPI may fall back to NTLM. NTLM is
      not implemented, so expect a prompt with reason "Negotiate failed" offering Basic. Record it.
- [ ] Wrong or expired ticket: expect the same prompt, never a silent Basic attempt.

## macOS / Linux
- [ ] `kinit user@REALM`, then open by FQDN: expect SSO.
- [ ] No ticket: expect the prompt with reason "Negotiate failed" and Basic offered.
- [ ] Short name or IP host: same as the Windows short-name case.

## Always
- [ ] Credentials never appear in logs (`Authorization`, `opaquelocktoken:`, `password=`).
- [ ] Basic is never sent over `http://`.
- [ ] Record results here with OS, client version, and server version.
