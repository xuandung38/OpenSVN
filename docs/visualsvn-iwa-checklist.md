# VisualSVN Server IWA (Negotiate) checklist

Manual verification of OpenSVN Negotiate (SSO) against a real VisualSVN Server.

## How it is built
- Windows: always on. SSPI `Negotiate` package through the `cross-krb5` crate (safe wrapper, no
  `unsafe` in OpenSVN). Only Kerberos is accepted (see "Kerberos only" below).
- macOS/Linux: system GSSAPI with the SPNEGO mechanism through `libgssapi`, behind the cargo feature
  `svn-ra/negotiate-gssapi` (off by default). Building it needs the gssapi headers (`libkrb5-dev` on
  Debian/Ubuntu, `krb5-devel` on Fedora; the macOS SDK has them) and libclang for bindgen. At run
  time it needs `libgssapi_krb5` (the deb already recommends `libgssapi-krb5-2`). Without the feature
  the client treats a Negotiate-only server as `negotiate-failed` and offers Basic when the server
  also offers it.
- https only: no Negotiate token is ever sent over `http://` (neither in answer to a challenge
  nor up front). A plain-http URL whose server offers Negotiate fails with `negotiate-failed`
  ("use an https:// URL"). There is no opt-in for http in the MVP.
- Flow: 401 with `Negotiate` → token for `HTTP/<host>` (built off the async runtime) → retry. When
  the server's 2xx carries `WWW-Authenticate: Negotiate <token>`, the token must pass mutual
  authentication or the response is discarded with `negotiate-failed`; a server that sends no
  final token is accepted (the token we sent was still checked to be Kerberos). After the first
  success each request starts Negotiate up front with a fresh context (tokens are single use).
  If an up-front token is rejected (clock skew, expired ticket), one fresh context answers the
  server's challenge before the host counts as failed. A transient retry (5xx, network error)
  always builds a fresh token; a token is never re-sent.
- Failures are remembered per host for 5 minutes; during that window the user gets the sign-in
  prompt with reason `negotiate-failed` (Basic, https only), and a server without Basic gives the
  error `negotiate-failed` with the underlying reason as a child error. After 5 minutes SSO is
  tried again, so a ticket obtained with `kinit` (or a renewed logon) is picked up without
  restarting.

## Service name (SPN)
- The service name always comes from the request URL host, never from the server: a trailing dot
  is stripped, the name is lowercased, and IP literals are refused before any Kerberos call
  (`negotiate-failed`: "needs the server's full host name"). Windows builds `HTTP/<host>`.
- macOS/Linux use the GSSAPI host-based name `HTTP@<host>` (`GSS_C_NT_HOSTBASED_SERVICE`). The
  Kerberos library turns that into `HTTP/<host>@REALM`, and with MIT krb5 the mapping depends on
  krb5.conf `[libdefaults]`:
  - `dns_canonicalize_hostname` (MIT default `true`; `fallback` from 1.18) resolves the host and
    follows CNAMEs, so the SPN can be the CNAME target rather than the URL host.
  - `rdns` (MIT default `true`) additionally reverse-resolves the address when canonicalizing.
  - DNS is not authenticated, so on hosts where that matters set
    `dns_canonicalize_hostname = false` and `rdns = false`; the SPN is then exactly the URL host.
    A server reached through a DNS alias then needs an SPN for the alias as well.
  - macOS (Heimdal-based GSS.framework) has its own canonicalization settings; record what the
    KDC log shows when testing.

## Kerberos only
- Both backends inspect their first token before it is sent. A raw Kerberos token, or a SPNEGO
  token whose first (optimistic) mechanism is Kerberos (`1.2.840.113554.1.2.2` or Microsoft's
  `1.2.840.48018.1.2.2`), is sent; anything else (NTLM, NegoEx, unknown) is refused with
  `negotiate-failed` and never leaves the machine. When SSPI cannot get a ticket for the SPN
  (short name, IP, missing SPN) it puts NTLM first, so this is where an NTLM fallback is stopped.
- When the server returns a final token, the completed context is checked again: on Windows
  `cross-krb5` queries `SECPKG_ATTR_NEGOTIATION_INFO` and rejects any package but Kerberos; on
  GSSAPI the negotiated mech OID (`gss_inquire_context`) must be Kerberos.
- Known limits: `cross-krb5` 0.5 does not expose `QueryContextAttributes` on a pending context,
  so on Windows a server that sends no final token is covered by the first-token check only
  (NTLM also cannot complete in one leg). `libgssapi` 0.11 does not expose `gss_set_neg_mechs`,
  so the SPNEGO mechanism list itself is not narrowed; SPNEGO is kept (rather than the raw
  Kerberos mech) because IIS/VisualSVN expect SPNEGO tokens. A machine whose SPNEGO lists NegoEx
  before Kerberos would be refused; record it if seen.

## Channel bindings (Extended Protection)
- For https, the client passes RFC 5929 `tls-server-end-point` channel bindings: the hash of the
  certificate the server presented on its latest response (SHA-256 for MD5/SHA-1/SHA-256
  signatures, otherwise the signature hash; none for Ed25519). This is what IIS/VisualSVN check
  when Extended Protection is `Required` (or `Accept`).
- Limits: the certificate is taken from the latest response for that origin, not from the exact
  pooled connection the next request uses. Behind a load balancer presenting different
  certificates, or behind a TLS-inspecting proxy, the bindings will not match and an Extended
  Protection server rejects the ticket (the client then prompts or reports `negotiate-failed`).

## NTLM decision
NTLM is not in the MVP. It needs several legs on one pinned connection, which the HTTP client does
not offer, and it is weaker than Kerberos. Short-name or IP URLs (no SPN match) therefore end in
the `negotiate-failed` prompt; use the server's full host name.

## Local KDC test (macOS/Linux)
`crates/svn-ra/tests/negotiate_gssapi_kdc.rs` is an ignored test that runs a real Kerberos round
trip (SPNEGO token check, acceptor with channel bindings, mutual authentication, mismatched
bindings refused, forged final token refused). It needs a KDC, a client ticket and an
`HTTP/<host>` keytab; see the file header for the command. It passed against MIT krb5 1.20.1 with
a throwaway local realm.

## Server side
- [ ] Server uses Windows authentication (IWA) and is reachable over https with a CA the client trusts.
- [ ] Note which providers the server offers: `curl -sI https://<host>/svn/<repo>` should show
      `WWW-Authenticate: Negotiate` and possibly `NTLM`; record the exact headers.
- [ ] SPN `HTTP/<fqdn>` is registered for the server's service account (`setspn -Q HTTP/<fqdn>`).

## Windows (domain-joined)
- [ ] Open the repository by FQDN: expect SSO with no prompt.
- [ ] Open by short name: SSPI may fall back to NTLM; the client refuses the NTLM token before
      sending it, so expect a prompt with reason "Negotiate failed" offering Basic. Record it.
- [ ] Open by IP: refused before any SSPI call; expect the same prompt (or `negotiate-failed`
      "use the full host name" when the server offers no Basic).
- [ ] Extended Protection `Required` on the server: expect SSO to work over https (channel
      bindings). Record the server's EPA setting.
- [ ] Open the repository by `http://` on a server offering Negotiate: expect `negotiate-failed`
      "use an https:// URL" and no `Authorization` header in a capture.
- [ ] Wrong or expired ticket: expect the same prompt, never a silent Basic attempt.

## macOS / Linux
- [ ] `kinit user@REALM`, then open by FQDN: expect SSO.
- [ ] No ticket: expect the prompt with reason "Negotiate failed" and Basic offered.
- [ ] Short name or IP host: same as the Windows short-name case.
- [ ] Record `dns_canonicalize_hostname` / `rdns` in krb5.conf; with a DNS alias URL check which
      SPN the KDC log shows.
- [ ] `kdestroy`, open (prompt), `kinit`, wait 5 minutes, open again: expect SSO without restart.

## Always
- [ ] Credentials never appear in logs (`Authorization`, `opaquelocktoken:`, `password=`).
- [ ] Neither Basic nor Negotiate credentials are ever sent over `http://`.
- [ ] Record results here with OS, client version, and server version.
