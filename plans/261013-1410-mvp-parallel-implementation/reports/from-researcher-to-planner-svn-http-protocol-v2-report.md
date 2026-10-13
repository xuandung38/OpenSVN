# SVN HTTPv2 / WebDAV protocol — pure-Rust client work packages

Date: 2026-10-13 · Scope: HTTP(S) only, server VisualSVN (mod_dav_svn), client behavior = libsvn_ra_serf
Source base: Subversion **trunk** (not a release tag). Pin to the tag matching VisualSVN's bundled mod_dav_svn before coding (see Q1).

## 0. Sources and credibility

- A (primary, authoritative): protocol notes `notes/http-and-webdav/http-protocol-v2.txt` (https://github.com/apache/subversion/blob/trunk/notes/http-and-webdav/http-protocol-v2.txt); `notes/svndiff` (https://github.com/apache/subversion/blob/trunk/notes/svndiff). Notes are terse and incomplete (no REPORT element list, no MERGE/lock/error format).
- A (primary, code = ground truth where notes are silent): libsvn_ra_serf client code, `options.c`, `update.c`, `commit.c`, `merge.c`, `lock.c`, `log.c`, `blame.c`, `getlocks.c`, `mergeinfo.c`, `serf.c`, `property.c` (https://github.com/apache/subversion/tree/trunk/subversion/libsvn_ra_serf); header constants `subversion/include/svn_dav.h`, `subversion/include/svn_error_codes.h`; server error writer `subversion/mod_dav_svn/util.c`; delta codec `subversion/libsvn_delta/svndiff.c`.
- B (secondary): Subversion book / dev list not consulted in this pass. No tutorials used.
- Rule: where notes and code disagree, code wins. Where only inferred from code path names, flagged **[unverified]**.
- Web calls used: 2 WebFetch (notes), rest via raw source download. No WebSearch.

## 1. Session open and capability negotiation

- Request: `OPTIONS <repo-root-URL>` (ra_serf `create_options_body`, `options.c`). Body: `D:options` XML (DAV: ns), child `D:activity-collection-set`.
- Server response headers that matter (`options.c` ~L256-320, `svn_dav.h`):
  - `SVN-Me-Resource` (presence = HTTPv2; ra_serf skips v2 if env var `SVN_IGNORE_V2_ENV_VAR` forces v1 — debug-only switch)
  - `SVN-Repository-Root`, `SVN-Repository-UUID`, `SVN-Youngest-Rev`
  - Stub templates: `SVN-Rev-Stub`, `SVN-Rev-Root-Stub`, `SVN-Txn-Stub`, `SVN-Txn-Root-Stub`, `SVN-VTxn-Stub`, `SVN-VTxn-Root-Stub`
  - `SVN-Supported-Posts` (comma list; `create-txn` assumed if absent)
  - `SVN-Allow-Bulk-Updates` (`on|off|prefer`)
  - `SVN-Repository-MergeInfo` (`yes|no`; server-side mergeinfo hint)
- Capability tokens: `DAV:` header, multi-valued, prefix `http://subversion.tigris.org/xmlns/dav/svn/` (`svn_dav.h` `SVN_DAV_NS_DAV_SVN_*`):
  - `depth`, `mergeinfo`, `log-revprops`, `atomic-revprops`, `partial-replay`, `inherited-props`, `ephemeral-txnprops`, `reverse-file-revs`, `inline-props`, `replay-rev-resource`, `svndiff1`, `svndiff2`, `list`, `put-result-checksum`
- Client also advertises the same tokens in its own request `DAV:` headers (`property.c` ~L901-903).
- Stub resolution: URIs are **templates from the server**, never hard-coded. `!svn` prefix is configurable (notes). If `SVN-Me-Resource` absent → fall back to classic DeltaV (`!svn/vcc`, `!svn/bc`, `!svn/wrk`, `!svn/act`) — deprecated; **recommend: refuse non-v2 servers in MVP** (YAGNI; VisualSVN ≥ 1.8 expected).
- Capability gating (serf): `supports_svndiff1/2`, `supports_inline_props`, `bulk_updates`, depth-filter need `depth` cap.

Resource URI forms (notes, verified):
- `!svn/me` (POST create-txn, REPORT update-style, dated-rev)
- `!svn/rev/REV` (rev props, PROPPATCH revprop)
- `!svn/rvr/REV/[PATH]` (revision root: GET file, PROPFIND dir/stat, REPORTs with pegrev)
- `!svn/txn/TXN` (txn props, MERGE, DELETE abort); `!svn/txr/TXN/[PATH]` (txn root: PUT/MKCOL/DELETE/COPY/PROPPATCH on nodes). Client builds these from `SVN-Txn-Root-Stub` (`commit.c` ~L1175-1190).
- `!svn/vtxn/...`, `!svn/vtxr/...` = alternate txn when client supplies UUID (`SVN-VTxn-Name` header).
- Public HEAD URL (no `!svn`): LOCK/UNLOCK, get-lock PROPFIND, get-locks REPORT.

## 2. Request sequence per operation

Notation: method URL — headers — body — response. "pegrev" = `!svn/rvr/REV/path` **[unverified exact choice per op; notes say "pegrev URI"]**.

**info / stat**
- `PROPFIND` pegrev URL, `Depth: 0`, body `D:propfind` → `D:prop` list (resourcetype, version-name, creationdate, creator-displayname, getcontentlength, checked-in). SVN props use ns `http://subversion.tigris.org/xmlns/svn/` (`SVN_DAV_PROP_NS_SVN`); unversioned custom props `.../xmlns/custom/`.
- Response: `D:multistatus` → `D:response` → `D:href`, `D:propstat/D:prop`. `version-name` = last-changed rev; `resourcetype` contains `D:collection` for dirs.
- check-path (node kind) = same PROPFIND, use `resourcetype` or 404.

**list (dir)**
- `PROPFIND` pegrev dir, `Depth: 1`, prop set as above + `D:displayname`/size. Server-side `svn/list` cap **[unverified]** may add extended listing; MVP = PROPFIND Depth 1.

**cat (file)**
- `GET` pegrev file URL → raw bytes; `Accept` none. Props (eol-style, keywords, executable, special) via PROPFIND. Stream to disk; do not buffer.

**log**
- `REPORT` on pegrev URL, `Depth: 0`, body `S:log-report` (`xmlns:S="svn:"`) children (from `log.c`): `S:start-revision`, `S:end-revision`, `S:limit`, `S:discover-changed-paths`, `S:strict-node-history`, `S:include-merged-revisions`, `S:path`, `S:revprop` (repeat) or `S:all-revprops` / `S:no-revprops`, `S:encode-binary-props`.
- Response: streamed `S:log-report` → `S:log-item` with children `D:version-name` (rev), `D:creator-displayname`, `S:date`, `D:comment`, `S:has-children`, `S:subtractive-merge`, `S:revprop name=…`, and path lists `S:added-path|S:modified-path|S:deleted-path|S:replaced-path` (attrs `copyfrom-path`, `copyfrom-rev`; text = path, action implied by element).
- Paging: client re-issues REPORT with `end-revision` lowered; stop at `limit`.

**blame**
- `REPORT` pegrev file, body `S:file-revs-report`: `S:path`, `S:start-revision`, `S:end-revision`, `S:include-merged-revisions`.
- Response: `S:file-rev path=… rev=…` per revision, children `S:rev-prop name=…`, `S:set-prop name=…`, `S:remove-prop name=…`, `S:merged-revision`, `S:txdelta` (base64 svndiff, delta from previous rev). Client reconstructs each text version by applying deltas, then runs line-blame (LCS) locally. Unsure whether `reverse-file-revs` cap changes order → implement forward order first.

**get-locks**
- `REPORT` public HEAD URL, body `S:get-locks-report`. Response `S:lock` → `S:path`, `S:token`, `S:owner`, `S:comment`, `S:expirationdate` (`getlocks.c`).

**LOCK / UNLOCK / get-lock**
- `LOCK` public URL, body `D:lockinfo` (`xmlns="DAV:"`) → `D:lockscope/D:exclusive`, `D:locktype/D:write`, `D:owner` (comment). Headers: `Timeout: Second-N` (parsed in `lock.c` ~L155), `If:` tagged-list when re-locking with known token (format `<url> (<opaquelocktoken:…>)`), `X-SVN-Options: lock-steal` for force (`svn_dav.h` `SVN_DAV_OPTION_LOCK_STEAL`).
- Response: `D:prop/D:lockdiscovery/D:activelock` → `D:locktoken/D:href` (`opaquelocktoken:` URI), `D:owner`, `D:timeout`.
- `UNLOCK` public URL, header `Lock-Token: <opaquelocktoken:…>`; `X-SVN-Options: lock-break` for admin break (`lock.c` L544, `svn_dav.h`).
- `unlock --force`: first get-lock via PROPFIND on public URL to learn token (`lock.c` ~L569-600).
- Lock conflict HTTP status **[unverified; expect 423 Locked]**; error body `SVN_ERR_FS_PATH_ALREADY_LOCKED` (160035).

**checkout / update / switch / status -u (update-report)**
- `REPORT` on `!svn/me` (update-style reports go to me-resource per notes). Body `S:update-report` (`xmlns:S="svn:"`). Children (`update.c` ~L2675-2740): `S:src-path` (URL), `S:target-revision`, `S:dst-path` (switch/relocation), `S:update-target`, `S:ignore-ancestry`, `S:send-copyfrom-args`, `S:recursive` (`no` only for depth files/empty), `S:text-deltas no` (when text not wanted), `S:depth`.
- **send-all vs skelta** (choose in `svn_ra_serf__do_update`, `update.c` ~L2616-2690):
  - Attribute `send-all="true"` on `S:update-report` = **send-all** (server sends full props + inline text-deltas). Chosen when server header `SVN-Allow-Bulk-Updates: prefer` or user option `http-bulk-updates=on` **[option name unverified]**, or server lacks inline-props.
  - Otherwise **skelta**: no send-all attr, plus `S:include-props yes`. Chosen when server says `off` or allows-but-doesn't-dictate (default) and inline-props supported.
  - ra_serf's default on modern servers = **skelta** (file contents not inline).
- Response (streamed XML, parse incrementally): `S:open-directory`, `S:add-directory`, `S:open-file`, `S:add-file` (attr `name`, plus `D:checked-in/D:href` URL of node at target rev), `S:delete-entry`, `S:absent-file|absent-directory`, `S:set-prop name=…`, `S:remove-prop`, `S:prop` (inline props in send-all/inline-props), `S:fetch-props` (skelta: props not inline → extra PROPFIND/GET), `S:fetch-file` (skelta: content not inline → GET `D:checked-in/D:href`), `S:txdelta` (base64 svndiff; send-all only; inside open-file/add-file), `S:target-revision`, closes `S:update-report`.
- Skelta flow for a changed file: stream response → on `S:fetch-file` queue GET on checked-in href (`!svn/rvr/REV/path`) → decode body directly as file content (no delta). Max parallel GETs ≤ `http-max-connections`.
- Send-all flow: `S:txdelta` base64 → decode base64 → svndiff windows → apply to base text (source = pristine) → new text.
- Note: `S:text-deltas no` is only a hint; server may still send deltas. Client must accept both.

**commit (HTTPv2)**
1. `POST !svn/me` body `( create-txn )` (skel s-expr). With revprops: `( create-txn-with-props ( ( name value ) … ) )` (`commit.c` ~L1103-1130). Response header `SVN-Txn-Name` (or `SVN-VTxn-Name` if client sent `SVN-VTxn-Name: <uuid>`). Build `txn_url` and `txn_root_url` from stubs (`commit.c` ~L1175-1190).
2. Edits, in order from the editor (depth-first): 
   - `MKCOL !svn/txr/TXN/path` (dirs).
   - `PUT !svn/txr/TXN/path` body = svndiff0/1 of text delta (empty body for empty file; add-without-history uses empty PUT). Headers: `X-SVN-Base-Fulltext-MD5` (base hex md5), `X-SVN-Result-Fulltext-MD5` (result hex md5 when server advertises put-result-checksum; server echoes checked value in response header — client compares `commit.c` ~L2040 and ~L2218), `Content-Type: application/vnd.svn-svndiff` **[unverified]**, `X-SVN-Version-Name` = base rev (out-of-date check, notes).
   - `DELETE !svn/txr/TXN/path`.
   - `COPY !svn/txr/TXN/src` headers `Destination: <absolute URL under txr/TXN/dst>`, `Depth: infinity` (`commit.c` ~L935, ~L1058-1060).
   - `PROPPATCH !svn/txr/TXN/path` body `D:propertyupdate` → `D:set/D:prop` or `D:remove/D:prop`; svn props use `S:` element names with prefix-stripped name (`commit.c` ~L552-603, base64 for non-XML-safe values).
   - Lock tokens: when node is locked, `If:` tagged-list header (`commit.c` ~L670-720, ~L1000-1010).
   - `X-SVN-Version-Name` on each write (base rev), notes.
3. Revprops (`svn:log`, author): `PROPPATCH !svn/txn/TXN` (notes: revprops set on txn resource) — or included in create-txn-with-props.
4. `MERGE !svn/txn/TXN` (`commit.c` ~L2254; `merge.c` create_merge_body): body `D:merge` (`xmlns:D="DAV:"`) → `D:source/D:href` (txn URL), `D:no-auto-merge`, `D:no-checkout`, `D:prop` (`D:checked-in`, `D:version-name`, `D:creationdate`, `D:creator-displayname`), `S:lock-token-list` → repeated `S:lock` (`lock-path` text, `lock-token` text) for each held lock. Header `X-SVN-Options` (e.g. `no-merge-response` per `svn_dav.h` **[verify which option is used in merge.c:288]**), `keep-locks` / `release-locks` (`svn_dav.h`).
   - Response: `D:merge-response` → `D:updated-set/D:response` (`D:href`, `D:propstat/D:prop` with `D:version-name` = **new revision**, `D:creator-displayname`, `D:creationdate`); `S:post-commit-err` = post-commit hook message (commit SUCCEEDED; surface as warning, `merge.c` L143).
5. On failure before MERGE or on abort: `DELETE !svn/txn/TXN` (`commit.c` ~L2315). Txn leak on crash — client should retry DELETE or let server GC; **[unverified server GC behavior]**.
- Error path for out-of-date: `SVN_ERR_FS_OUT_OF_DATE` (160042) or conflict (160024) — see §5.

**mergeinfo-report**
- `REPORT` pegrev URL, body `S:mergeinfo-report` children: `S:revision`, `S:inherit` (explicit|inherited|nearest-ancestor), `S:include-descendants`, `S:path` (repeat). Element names from `svn_dav_protocol.h`: `mergeinfo-report`, `mergeinfo-item`, `mergeinfo-path`, `mergeinfo-info`. Response `S:mergeinfo-item` with `mergeinfo-path` (path; strip leading `/` — server may send relative paths, `mergeinfo.c` L103-106) and `mergeinfo-info` (mergeinfo text, parse with svn mergeinfo grammar).
- Gated by `SVN-Repository-MergeInfo` header / `mergeinfo` cap.

**get-location-segments** — **[unverified]** not fetched (locations.c not present at `subversion/libsvn_ra_serf/locations.c` path tried). Notes list it as REPORT on pegrev URI. Get element names from `subversion/libsvn_ra_serf/` source before implementation.

## 3. svndiff (notes/svndiff, svndiff.c)

- Header: 4 bytes `"SVN"` + version byte: `0`, `1` (zlib), `2` (LZ4).
- Window = `sview_offset`, `sview_len`, `tview_len`, `inst_len`, `newdata_len`, instructions bytes, new-data bytes. All integers = big-endian varint, MSB continuation bit, higher-order groups first (130 = `0x81 0x02`).
- Instruction byte: top 2 bits = op (`00` copy source view, `01` copy target view, `10` copy new data, `11` invalid); low 6 bits = length; if 0, length follows as varint.
  - op 00/01: then varint offset (source-relative / target-relative).
  - op 10: no offset; consumes next N bytes of new data sequentially.
  - Target-view copy may overlap its own output (byte-by-byte repeat semantics).
- Compression: v1 zlib, v2 LZ4. Each of instructions/new-data stored as `[varint original_len][payload]`; if `original_len == stored_len − prefix_len` payload is raw. **[conflict: notes list original-length as separate window fields for v1; svndiff.c `decode_window` only receives `inslen`/`newlen` and decompresses sections. Trust code; confirm with a real v1/v2 fixture.]**
- Window stream ends at end of document (external length known: base64 length in XML, or HTTP Content-Length).
- Encoder: v0 uncompressed is always valid. Negotiation (`commit.c` `negotiate_put_encoding` ~L1869-1934): with `http-compression=auto`, prefer v2 if server supports, then v1, else v0; compression disabled → v0.
- Decoder must accept v0/v1/v2 (server choice in txdelta).
- Test: `svnadmin dump --deltas` on a fixture repo emits svndiff payloads in dumpfile form **[unverified format detail]** — good cross-check generator.

## 4. XML namespaces and mandatory elements

- Namespaces: `DAV:` (`D:`), SVN protocol `svn:` (`S:`, request bodies `xmlns:S="svn:"`), mod_dav error `http://apache.org/dav/xmlns` (`m:`), SVN error `svn:` (`C:`), SVN props `http://subversion.tigris.org/xmlns/svn/`, custom props `…/xmlns/custom/`, DAV props `…/xmlns/dav/`.
- Mandatory in requests: XML prolog; root element with correct `xmlns` for `D:` and `S:`; `D:` elements as in §2. Unknown response elements must be **ignored** (forward-compat; ra_serf uses state tables).
- Mandatory in response parsing: `D:multistatus` (PROPFIND), `D:merge-response` (MERGE), `S:update-report` / `S:log-report` / `S:file-revs-report` / `S:get-locks-report` (REPORT). Parse streaming (`quick-xml` reader), never build DOM.
- Element names are case-sensitive; attributes like `send-all`, `name`, `path`, `rev`, `copyfrom-path`, `copyfrom-rev`, `errcode`.

## 5. Error response format

- Writer (`mod_dav_svn/util.c` ~L775-794), HTTP status = mapped DAV status:
  ```
  <?xml …?>
  <D:error xmlns:D="DAV:" xmlns:m="http://apache.org/dav/xmlns" xmlns:C="svn:">
  <C:error/>
  <m:human-readable errcode="160013">message text</m:human-readable>
  </D:error>
  ```
  - `C:error` tag = namespace `svn:` tag `error` (`svn_dav.h` SVN_DAV_ERROR_NAMESPACE/TAG); `m:human-readable/@errcode` = svn error code. Nested `svn:error` chains may appear (`util.c` L134 comment).
- SVN error codes = `APR_OS_START_USERERR` (120000) + category×5000 + offset (`svn_error_codes.h`). Verified values:
  - FS_CATEGORY_START 160000 (8×5000): NO_SUCH_REVISION 160006, NOT_FOUND 160013, NOT_DIRECTORY 160016, ALREADY_EXISTS 160020, CONFLICT 160024, PATH_ALREADY_LOCKED 160035, LOCK_OWNER_MISMATCH 160039, NO_SUCH_LOCK 160040, OUT_OF_DATE 160042.
  - RA_DAV_CATEGORY_START 175000 (11×5000): RA_DAV_REQUEST_FAILED 175002, PATH_NOT_FOUND 175007 (serf replaces with FS_NOT_FOUND).
- Client error mapping rule: parse errcode first; fall back to HTTP status when body missing/not XML (proxies, auth pages).

## 6. Auth, TLS, connections

- Auth challenge: `401 Unauthorized` + `WWW-Authenticate: Basic realm="…"` or `WWW-Authenticate: Negotiate` (SPNEGO; client retries with `Authorization: Negotiate <base64 token>`; server may return a final token in `WWW-Authenticate`). NTLM also possible via SSPI on Windows. Serf's authn types are set from config (`serf.c` ~L133, `serf_config_authn_types` ~L451). **[exact header samples not captured; verify with fixture]**
- Recommendation (per ADR-003): Basic over TLS first (VisualSVN default, AD accounts); Negotiate second (SSO); credentials cached per realm; retry 401 once with credentials; never send Basic over plain http.
- TLS: rustls + platform verifier (ADR-003). Internal CA must be trusted via OS store.
- Connections (`serf.c`, `ra_serf.h`):
  - `http-max-connections` default **4** (`svn_config.h` L239), clamped to [2, 8] (`serf.c` L364-367; `SVN_RA_SERF__MAX_CONNECTIONS_LIMIT` 8 in `ra_serf.h` L87).
  - HTTP/1.1 keep-alive, persistent connection per slot; `http-chunked-requests` default auto (`serf.c` L430-445): chunked request bodies allowed, falls back to Content-Length for proxies that reject chunked (`serf.c` ~L625).
  - Timeout: `http-timeout` (`serf.c` ~L370, default in `DEFAULT_HTTP_TIMEOUT` **[value unverified]**).
  - Pipelining: out of scope in notes.

## 7. Work-package split (sans-IO core + transport adapter)

Principle (ADR-003): protocol layer = pure functions (request builders return method/URL/headers/body-stream; parsers take byte streams). Transport (reqwest/hyper) only in WP-T.

- **WP-1 xml-streaming** — quick-xml reader/writer helpers, namespace-aware state machine, unknown-element skipping. Deps: none. Test: unit + fuzz.
- **WP-2 svndiff-codec** — v0 encode/decode, v1 zlib, v2 LZ4, varint, window apply to source. Deps: none. Test: golden windows (from notes example "aaaabbbbcccc"→"aaaaccccdddddddd"), fuzz decoder, round-trip.
- **WP-3 dav-session-caps** — OPTIONS build/parse, DAV cap set, stub templates → URL builders, v2 gate, POST support set. Deps: WP-1. Test: header fixtures.
- **WP-4 read-propfind-get** — PROPFIND (stat, list, get-dir, locks lookup), GET (cat, skelta fetch). Deps: WP-1, WP-3. 
- **WP-5 reports-readonly** — log, file-revs (blame), get-locks, mergeinfo. Deps: WP-1, WP-2 (blame txdelta), WP-3. Can split into 5a log/locks/mergeinfo and 5b blame.
- **WP-6 update-engine** — update-report request builder (send-all/skelta choice), streaming response → editor events (add/open/delete/set-prop/txdelta/fetch-file), fetch scheduler. Deps: WP-1, WP-2, WP-3, WP-4. Highest complexity; biggest fixture need. Consumer = WC engine (not in this report).
- **WP-7 commit-txn** — create-txn POST, edit ops (MKCOL/PUT/DELETE/COPY/PROPPATCH), checksums, If headers, MERGE body/response parse, abort DELETE. Deps: WP-1, WP-2 (encoder), WP-3, WP-8 (lock tokens).
- **WP-8 locks** — LOCK/UNLOCK/get-lock, token store helper for If/Lock-Token. Deps: WP-1, WP-3.
- **WP-9 errors** — D:error/m:human-readable parser, errcode table, HTTP-status fallback. Deps: WP-1. Small; can start day 1.
- **WP-10 auth-tls** — Basic, Negotiate (cross-krb5), 401 retry, credential cache hook, TLS roots. Deps: WP-T. Independent of protocol WPs. Highest environment risk (Kerberos SPN on Windows domain).
- **WP-T transport** — reqwest/hyper client, HTTP/1.1 pool (max 4, clamp 2–8), chunked-body toggle, timeouts, proxy. Deps: none. Thin adapter; only I/O crate.
- **WP-F fixtures/harness** — capture tooling + sans-IO golden test runner. Deps: none; needed by all WPs.

Dependency order: WP-F, WP-1, WP-2, WP-9, WP-T (parallel) → WP-3 → WP-4, WP-5, WP-8 (parallel) → WP-6, WP-7 → WP-10 integration.

## 8. Fixture capture (ranked)

1. **Real server, plain HTTP, Apache 2.4 + mod_dav_svn (same svn minor as VisualSVN)** on macOS (Homebrew `httpd`, `subversion`) or Docker. Mirror VisualSVN auth (Basic via mod_auth_basic; Negotiate via mod_auth_gssapi if testing SSO). Plain HTTP keeps fixtures readable; TLS only in integration tests.
   - Capture client traffic with **mitmproxy in forward-proxy mode**; point svn at it via `servers` config `[global] http-proxy-host` / `http-proxy-port` **[verify key names]**. Save raw request/response per op as golden files.
   - Normalize: Date, ETag, nonces, Authorization, UUIDs, timestamps, txn names (store placeholders).
2. **Server-side body dump**: Apache `mod_dumpio` (if the httpd build has it) or `LogLevel` debug on mod_dav_svn for server-side view **[unverified availability]**.
3. **svnadmin dump --deltas** for svndiff payload cross-check (no network).
4. **Wireshark / tcpdump** only for TLS handshake/Negotiate sanity; not for XML goldens (encrypted).

Rule: do not hand-write expected XML from notes; derive goldens from captures + source. Keep fixtures under `tests/fixtures/` with a README of server version and svn version.

## 9. Pitfalls

- Notes ≠ code. Notes omit REPORT element names, MERGE/lock/error formats, skelta details — all taken from source; re-verify against the pinned release tag.
- Stubs and `!svn` prefix are server-chosen; any hard-coded `!svn/...` breaks custom Apache `SVNPathPrefix`/config.
- Skelta vs send-all: both must be handled; the server may ignore `text-deltas no`.
- Skelta fetch scheduling: fetch-file/fetch-props arrive interleaved with the update stream; holding all requests until end deadlocks/grows memory. Bound concurrency by pool size.
- svndiff v1 "original length" layout is ambiguous (notes vs code). Get a real v1/v2 fixture before coding decoder for them.
- Base64 txdelta in XML: decode streaming; chunk boundaries not aligned to 4 bytes.
- MERGE success with `S:post-commit-err` = commit happened. Do not retry commit.
- Out-of-date commit: PUT/MERGE can fail late (MERGE), not early. Clean up txn via DELETE.
- `If:` header tagged-list needs the full URL and encoded token; wrong format → 412/423 with no clear error **[unverified status]**.
- Revprop writes require server support (`atomic-revprops` cap) and usually admin rights; VisualSVN pre-revprop-change hook decides.
- Chunked request bodies: some corporate proxies break them; keep the fallback to Content-Length (serf does this).
- Mergeinfo paths may come with leading `/` or relative — normalize.
- Auth: Negotiate over HTTP with Kerberos needs SPN `HTTP/<host>`; macOS GSSAPI vs Windows SSPI behavior differs (ADR-003 risk).
- HTTP 401 must not loop: cap retries; never persist Basic password in plaintext (PRD constraint).
- Error bodies may be HTML (IIS/Apache error pages, proxies); never assume XML.
- Server GC of abandoned txns **[unverified]**; client must not rely on it.

## 10. Adoption risk and trade-offs (implementation approach)

| Option | Perf | Complexity | Maintenance | Fit (ADR-003) | Rank |
|---|---|---|---|---|---|
| A. Own sans-IO protocol layer over reqwest/hyper, source-derived | good | medium-high (~10 WPs) | owned by us; protocol stable since 1.8 (low breaking-change risk) | matches pure-Rust rule | **1** |
| B. FFI to libsvn_ra_serf / libsvn_* (C) | best parity | low code, high build (apr, serf, sqlite, OpenSSL) | depends on C build per OS | violates pure-Rust (ADR-001/003) | 2 (reference only, for fixtures) |
| C. Shell out to `svn` CLI | fine | low | external dependency on user install | breaks WC-engine ownership | 3 (not acceptable) |

Adoption risk of A: no mature pure-Rust SVN client identified in this pass (not searched — **unverified**). Protocol churn is low (v2 unchanged since 1.8 per notes), but Subversion 1.15+ additions (cap tokens) must be feature-gated, not assumed.

## 11. Limitations (not covered)

- Release-tag pinning: all source read from trunk. Behaviors (cap tokens, element names) may differ in the release VisualSVN ships.
- Not fetched: get-location-segments, get-dated-rev, replay, get-deleted-rev, get-file-revs-reverse, inherited-props, rev-prop/rev-proplist details, MERGE response edge cases, lock conflict status codes, mod_dav_svn auth path, actual HTTP header samples for 401/Negotiate.
- No live server run; nothing captured. All wire examples in §2 are from client source, not observed traffic.
- VisualSVN-specific behavior (its Apache version, mod_authz_svn, hooks, bulk-update setting, SVNPathPrefix) not verified — no VisualSVN docs read.
- Client-side WC/editor semantics (libsvn_wc, delta editor ordering) out of scope; WP-6/7 interfaces need a separate design.
- No code written, no benchmarks.

## 12. Unresolved questions

1. Exact VisualSVN release and bundled Apache/mod_dav_svn version? Determines pinned tag and whether svndiff2/inline-props/reverse-file-revs caps are present.
2. Does VisualSVN enable `SVN-Allow-Bulk-Updates` = `prefer`/`off`/none? Decides default update mode the client must support first.
3. Exact REPORT bodies/responses for get-location-segments, get-dated-rev, get-mergeinfo edge cases — need locations.c / replay.c / mergeinfo details (not fetched).
4. Precise svndiff v1 layout (notes vs svndiff.c) — need real fixture.
5. Which X-SVN-Options value merge.c uses (no-merge-response vs keep-locks) and its effect on MERGE response size.
6. Lock conflict HTTP status and retry semantics; `Timeout` default for LOCK.
7. Does the MVP need DeltaV (v1) fallback at all? Recommended no; confirm with product.
8. Negotiate: does VisualSVN require Kerberos SPN config; who owns AD/SPN setup? (ADR-003 risk.)
9. Pipelining/keep-alive limits behind corporate proxies for VPN users (1–50 ms LAN per design doc).
10. Blame `reverse-file-revs` capability: which order does the server send without it?

Status and summary are reported in the final message to the lead.
