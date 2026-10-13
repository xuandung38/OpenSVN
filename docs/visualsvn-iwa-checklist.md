# VisualSVN Server IWA (Negotiate) checklist

Manual verification of OpenSVN Negotiate (SSO) against a real VisualSVN Server. Negotiate itself is
still being implemented; this list is what the product owner should try once it lands.

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
