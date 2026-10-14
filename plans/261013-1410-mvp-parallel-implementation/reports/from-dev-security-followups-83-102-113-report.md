# From dev: security follow-ups #83, #102, #113

Branch `fix/security-followups-83-102-113`, PR #114.

## #83 svn-ra Negotiate
- `check_kerberos_token` now parses the whole SPNEGO `NegTokenInit`. The first `mechTypes` entry
  must be Kerberos (MIT or MS OID). The optimistic `mechToken` must be a raw Kerberos token:
  Kerberos OID, `TOK_ID` 01 00, and exactly one `[APPLICATION 14]` AP-REQ. Optional fields must
  appear at most once and in order (`[1]`..`[4]`; `[4]` covers the MS-SPNG NegTokenInit2 layout).
  The outer token must be exactly one TLV. Anything else fails closed: a missing mechToken,
  trailing bytes, truncation at any length, a non-OCTET STRING mechToken, unknown tags.
  An NTLM inner token, raw or wrapped, reports "the system offered NTLM".
- `tls_server_end_point` no longer calls `SignatureAlgorithm::try_from` except for RSASSA-PSS.
  x509-parser does not classify the NIST DSA OIDs (`2.16.840.1.101.3.4.3.*`), so DSA-with-SHA256
  certificates used to produce no binding. Hashes added per RFC 5929: SHA-224 (RSA, ECDSA, DSA,
  bare OID), and dsa-with-SHA1/SHA256/384/512. Test certificates rcgen cannot make sit in
  `crates/svn-ra/src/transport/testdata/` (generated with OpenSSL; the README has the commands).

## #102 svn-protocol
- `check_server_url_text` refuses leading or trailing whitespace (`char::is_whitespace`, so
  NBSP and U+3000 too).
- `raw_path_of` treats `://` as a scheme separator only when the value starts with an RFC 3986
  scheme. `accept_href` uses the same helper.
- Evil fixtures added: `update/href-trailing-space-dotdot`, `update/href-encoded-dotdot-space`,
  `update/href-scheme-in-path`, `propfind/child-dotdot-space`, `propfind/child-scheme-in-path`,
  `options/stub-scheme-in-path`, `options/root-scheme-in-path`. Running `generate.py` twice
  rewrites no existing file. On the old code, `options/stub-scheme-in-path` was accepted.
  The other fixtures were already caught by other layers (callers trim, `classify`).

## #113 auth
1. Each flight in svn-ra records the `(method, URL)` pairs that challenged (at most 256). A
   later request to one of them that sends the credentials up front counts as challenged. So a
   typed password whose resent request got a 5xx, a network error or was cancelled is
   confirmed by the next success there. A different URL or a different method does not count:
   servers often allow anonymous GET but require auth for writes. Logging covers a 5xx with an
   unconfirmed password, a typed password replaced by the stored one, and an unconfirmed
   password still held when the `AuthManager` is dropped. Tests cover the 5xx case and the
   cancel case.
2. `AuthBroker`: the sign-out counter mutex (now `Arc`) is the single gate. `credentials_accepted`
   runs the count check, the keychain write, `account_upsert` and the replaced-secret cleanup
   in one `spawn_blocking` under that gate. `sign_out` holds it for the bump and all deletions.
   **Deviation from the issue text:** the issue asks to test the "delete after race" branch.
   With one lock held across the keychain write that branch can no longer run, so I removed it
   rather than keep dead code. A narrower lock (keychain write outside the lock, plus the
   delete) would still lose data: a newer save typed after the sign-out could be deleted. The
   new test `a_sign_out_during_the_keychain_write_removes_the_saved_password` covers exactly
   that scenario: it stalls the keychain write, starts a sign-out, checks that the sign-out
   waits, then checks that neither the secret nor the row survives.
   After review: the count lives in its own short-held lock and a separate `account_gate`
   serializes save vs sign-out (count bumped only under the gate), so answering a prompt never
   waits for a slow keychain write; the race test also checks that.
3. `answer_unauthorized` reads `seq` at each of its three `Ok` returns while it holds the
   flight gate. `on_unauthorized` no longer overwrites it. There is no dedicated test: the
   race cannot be reproduced deterministically without injection hooks. The existing denial
   tests still pass.

## Notes
- Commit trailers use the session URL given in the task brief.
- svn installed from apt is 1.14.3. Tests that need 1.15 skip.
- No frontend changes.
