# svn-protocol notes: what was verified against real traffic

Scope of this file: facts about the Subversion HTTPv2 (mod_dav_svn) wire protocol that the
`svn-protocol` goldens depend on. "Verified" means observed in a recording under `tests/data/`
(Apache 2.4.68 + mod_dav_svn **1.14.5**, client **svn 1.15.0** / serf 1.3.10). Everything else is
marked unverified. Behaviour of a 1.15 server and of VisualSVN is not yet recorded.

## How fixtures are captured

```
crates/svn-protocol/tools/capture/
  Dockerfile dav_svn.conf entrypoint.sh   throwaway Apache + mod_dav_svn (Debian trixie), Basic auth
  proxy.py                                message-level logging proxy: svn -> proxy -> container
  run.sh                                  runs ~45 svn commands, one jsonl per command
  extract.py                              picks exchanges, anonymizes, writes tests/data/
```

```
crates/svn-protocol/tools/capture/run.sh /tmp/raw          # needs docker, svn, python3
python3 crates/svn-protocol/tools/capture/extract.py /tmp/raw crates/svn-protocol/tests/data
```

* `proxy.py` forwards plain HTTP, forces `Accept-Encoding: identity` upstream and logs decoded
  request/response bodies (chunked request bodies are re-assembled).
* `extract.py` rewrites `127.0.0.1:18081` to `svn.test.local`, drops `Authorization`, `Date`,
  `Content-Length`, `Transfer-Encoding`, `Connection`, `Host`, `Server`, `Vary`.
  The repository UUID and lock tokens are random per run and kept as recorded; no real host or
  account appears (test users `alice`/`bob` only). Re-running the capture changes timestamps,
  UUIDs and lock tokens; the tests assert structure and the `svn --xml` cross-check, not those values.
* Fixture layout: `<name>.meta` (request line, status, headers; `>` request, `<` response),
  `<name>.req` / `<name>.resp` (raw bodies), `<name>.svn.xml` (output of the reference CLI).
* `list-depth1` was produced with `curl` using the exact PROPFIND body svn sends for stat,
  because svn 1.15 lists directories with `S:list-report` against a server that advertises `list`.
* Quirk: server answers listings in hash order, not sorted.
* P06 will provide the official harness and a 1.15 server; this tooling is deliberately minimal.

### Fuzzing

Targets live in `fuzz/fuzz_targets/` (standalone crate, needs a nightly toolchain):

```
cd crates/svn-protocol/fuzz
cargo +nightly fuzz build                 # compile all targets
cargo +nightly fuzz run multistatus -- -max_total_time=3600
```

The 1h-per-target run belongs to the nightly CI job; the stable test suite covers the same
entry points with `tests/mutation_smoke.rs`.

## Verified facts

### OPTIONS / session
* Request `OPTIONS <session url>`, `Content-Type: text/xml`, three `DAV:` headers
  (`.../svn/depth`, `.../svn/mergeinfo`, `.../svn/log-revprops`), body
  `<D:options xmlns:D="DAV:"><D:activity-collection-set/></D:options>`.
* `DAV:` capability tokens come in several header lines and comma lists (`1,2`,
  `version-control,checkout,...`); tokens of interest start with
  `http://subversion.tigris.org/xmlns/dav/svn/`.
* **Stub headers carry server-absolute paths, not URLs**: `SVN-Me-Resource: /svn/repo/!svn/me`,
  `SVN-Rev-Root-Stub: /svn/repo/!svn/rvr`, `SVN-Rev-Stub`, `SVN-Txn-Root-Stub`, `SVN-Txn-Stub`,
  `SVN-VTxn-*`, and `SVN-Repository-Root: /svn/repo`. `UrlBuilder` resolves them against the
  session origin and also accepts absolute URLs on the same origin.
* `SVN-Supported-Posts` is sent as **two header lines** (`create-txn`, `create-txn-with-props`).
* A plain `OPTIONS` without the activity body returns none of the `SVN-*` headers: only the first
  OPTIONS of a session is informative.
* Unauthenticated requests get `401`, `WWW-Authenticate: Basic realm="..."` and an **HTML** body.

### Which requests carry the `DAV:` capability headers
Observed: OPTIONS, REPORT, LOCK, UNLOCK, POST, PUT, MKCOL, MERGE send them. **PROPFIND and GET do
not.** The builders follow this (test `wire_parity`).

### PROPFIND
* Stat/list body: `resourcetype`, `getcontentlength`, `deadprop-count` (ns
  `http://subversion.tigris.org/xmlns/dav/`), `version-name`, `creationdate`,
  `creator-displayname`; `Depth: 0|1`, `Content-Type: text/xml`.
* URL is the peg-revision resource `!svn/rvr/<rev>/<path>`. HEAD is resolved by the client from
  `SVN-Youngest-Rev` of OPTIONS; the builders always take an explicit revision.
* Directory hrefs end with `/`; files do not. Responses carry a second `propstat` with
  `404 Not Found` for `getcontentlength` on directories.
* `allprop`: user properties appear as `C:<name>` (custom ns, name may contain `:`),
  versioned ones as `S:<name>` (svn ns); DAV-level extras (`md5-checksum`, `sha1-checksum`,
  `repository-uuid`, `baseline-relative-path`, `deadprop-count`) live in the `.../xmlns/dav/` ns.
* `checked-in` in an allprop response points at `!svn/ver/<rev>/...` (a DeltaV resource). The URL
  vetting treats everything under the stub directory that is not a known stub as unsafe.
* Missing node: plain `404` with an HTML body (not a `D:error`).
* get-lock: `PROPFIND` on the **public** URL, `Depth: 0`, `<prop><lockdiscovery/></prop>`.
  Unlocked node: `<D:lockdiscovery/>` with status 200.

### GET
`GET rvr/<rev>/<path>`, no body, no `DAV:` headers; response is the raw file.

### log-report
* `REPORT rvr/<peg>/<path>`, `Content-Type: text/xml`, no `Depth` header, no XML declaration.
  Element order: `start-revision`, `end-revision`, `limit`, `discover-changed-paths`,
  `include-merged-revisions`, `revprop*` / `all-revprops`, `path`, `encode-binary-props`.
  `strict-node-history` placement is from client source, not observed.
* Response items are flat `S:log-item`; **element order inside an item varies** (paths may come
  before or after `D:version-name`). Author is `D:creator-displayname`, date `S:date`, message
  `D:comment`; an item without author/message simply omits them.
* Revision 0 is returned like any other and has no author; the CLI hides it when it has no message.
* Merged revisions (`-g`): children follow the item that has `S:has-children`, terminated by
  an item whose `D:version-name` is `-1`. `LogItem::merge_depth` models the nesting.
* Paths: `S:added-path|modified-path|deleted-path|replaced-path`, attributes `node-kind`,
  `text-mods`, `prop-mods`, and `copyfrom-path`/`copyfrom-rev` on copies.
* Resolving a file's peg revision first costs one extra REPORT
  `S:get-locations` -> `S:get-locations-report/S:location@rev,@path` (client side concern).

### Locks
* `LOCK` on the public URL; body `lockinfo` with `exclusive`/`write` and `owner` = **the comment**.
  Steal: `X-SVN-Options: lock-steal`. From a working copy the client also sends
  `X-SVN-Version-Name: <wc revision>`. No `Timeout` header is sent by svn.
* Response 200: `D:prop/D:lockdiscovery/D:activelock` (`locktoken/href` =
  `opaquelocktoken:<uuid>`, `owner` = comment, `timeout` = `Infinite`). **The lock owner and
  creation time are headers**: `X-SVN-Lock-Owner`, `X-SVN-Creation-Date`; token also in `Lock-Token`.
* Conflict: **HTTP 423** with `D:error` errcode **160035** (`FS_PATH_ALREADY_LOCKED`).
* `UNLOCK`: `Lock-Token: <opaquelocktoken:...>` (with angle brackets), 204;
  `X-SVN-Options: lock-break` to break.
* get-locks-report: `REPORT` on the public URL, `<S:get-locks-report xmlns:S="svn:" depth="..."/>`
  (`empty|files|immediates|infinity`); items `S:lock` with `path` (repository-absolute), `token`,
  `creationdate`, `owner`, optional `comment`, optional `expirationdate`.

### Errors
* `D:error` carries `C:error/` and `m:human-readable@errcode`. Observed: lock conflict 160035 on
  HTTP 423; **out-of-date PUT is HTTP 409 with errcode 170004 (`SVN_ERR_RA_OUT_OF_DATE`)**, not
  160042. Both map to `OutOfDate`.
* No observed response contained more than one `human-readable` element; chain support follows
  the report and is covered by synthetic tests only.
* The errcode table in `errors.rs` was generated from `svn_error_codes.h` of 1.15.0.

### update-report (verified with svn 1.15.0 against 1.14.5)

* `REPORT !svn/me`. Body: optional `send-all="true"` attribute, `S:include-props yes` (skelta
  only), `S:src-path` (server path of the session URL), `S:target-revision` (omitted for HEAD),
  `S:dst-path` (**absolute URL**, switch only), `S:update-target`, `S:text-deltas no`
  (status -u), `S:depth` (`unknown` when unspecified), then one `S:entry` per reporter call.
  `ignore-ancestry` and `send-copyfrom-args` were not observed; their position follows the client
  source.
* Entries: `rev`, `depth` (omitted when unknown), `start-empty="true"`, `lock-token`
  (`opaquelocktoken:...`), and for switched subtrees `linkpath="/branches/b1"` (repository path,
  leading slash); the text is the path relative to the update root, empty for the root.
  Attribute order differs between operations: compare as XML, not as bytes. `S:missing` for
  `delete_path` follows the client source, not an observation.
* Answer: `S:target-revision@rev`, nested `open/add-directory` and `open/add-file` (`name`,
  `rev`, `sha1-checksum`; `bc-url` is ignored), `D:checked-in/D:href` (`!svn/rvr/<rev>/..`),
  `S:set-prop` (also `svn:entry:*`), `S:txdelta` (base64 svndiff, send-all only),
  `S:prop/V:md5-checksum`, `S:delete-entry`. With `update-target` the root `open-directory` has
  no `name` and the target appears as a child.
* `S:fetch-file` and `S:fetch-props` (servers without `inline-props`) and `S:absent-*` are
  parsed per the client source and covered by synthetic tests only.

### commit (verified)

* `POST !svn/me`, `application/vnd.svn-skel`:
  `(create-txn-with-props (name value ...))`, bare atoms for names, `<len> <bytes>` otherwise;
  answer `201` with `SVN-Txn-Name`.
* `COPY` is addressed to the **source** (`rvr/<rev>/<src>`) with `Destination: <absolute txr URL>`
  and `Overwrite: F`; no `Depth` header was sent.
* `MKCOL`/`DELETE`/`PROPPATCH`/`PUT` address `!svn/txr/<txn>/<path>`; `X-SVN-Version-Name` is the
  base revision (absent on MKCOL and on adds). PUT sends `X-SVN-Base-Fulltext-MD5` for existing
  files and no result checksum; the answer carries `X-SVN-Result-Fulltext-MD5`.
* PROPPATCH body: `D:propertyupdate` declaring `D`, `V`, `C`, `S`; `svn:x` is `S:x`, other names
  `C:<full name>`; unsafe values would use `V:encoding="base64"` (builder-tested, not observed).
* Locked path: `If: <public URL> (<opaquelocktoken:...>)`, and the `MERGE` body gets
  `<S:lock-token-list xmlns:S="svn:"><S:lock><lock-path>..</lock-path><lock-token>..</lock-token></S:lock>..`
  with **unprefixed** `lock-path`/`lock-token`, the path relative to the merge root. A PUT on a
  locked path without the header gets plain `423` HTML.
* `MERGE` goes to the public URL of the commit root; `X-SVN-Options: release-locks
  no-merge-response`; the answer names the new revision in the `D:response` whose resourcetype is
  `D:baseline`. Abort is `DELETE !svn/txn/<txn>` (204).

### blame (verified)

`REPORT rvr/<peg>/<file>`: `S:file-revs-report` with `start-revision`, `end-revision`, `S:path`
(empty). Answer: `S:file-rev@path,@rev` containing `S:rev-prop@name`, `S:set-prop@name`,
`S:txdelta` (base64 svndiff against the previous version). `include-merged-revisions` position
and `S:merged-revision` / `S:remove-prop` are from the client source.

## Capabilities and fallbacks (mod_dav_svn 1.8 - 1.15, HTTPv2)

Behaviour is chosen from the `OPTIONS` answer only, never from a version string. Releases are
approximate (Subversion release notes, not re-verified here); the capability token is what counts.

| Capability / header | About since | Used for | When absent |
|---|---|---|---|
| `SVN-Me-Resource` + stubs | 1.7 | everything | refused with `ServerUnsupported` |
| `depth`, `mergeinfo`, `log-revprops` | < 1.8 | sent on our requests | assumed present (all 1.8+) |
| `list` | 1.10 | `list-report` listing | `Depth: 1` PROPFIND (`Capabilities::list_strategy`) |
| `svndiff1`, `svndiff2` | 1.10 | svndiff version of PUT | version 0 (`svndiff_version`) |
| `inline-props` | 1.8 | skelta `include-props` | skelta without it; answer has `fetch-props`/`fetch-file` |
| `SVN-Allow-Bulk-Updates` | 1.8 | send-all vs skelta | header absent = no restriction; `Off` forces skelta |
| `put-result-checksum` | 1.8 | `X-SVN-Result-Fulltext-MD5` on PUT | header not sent (`PutOptions::with_result_md5`) |
| `SVN-Supported-Posts: create-txn-with-props` | 1.8 | revprops with the txn | plain `create-txn` + PROPPATCH on the txn (`create_txn_plan`) |
| `ephemeral-txnprops` | 1.8 | `svn:txn-client-*` txn props | none sent (`ephemeral_txn_props`) |
| `reverse-file-revs` | 1.8 | descending file-revs range | ask ascending, caller reverses (`plan_file_revs`) |
| `inherited-props`, `replay-rev-resource`, `atomic-revprops`, `partial-replay` | 1.5 - 1.8 | not used by this crate | not requested |
| `SVN-Repository-MergeInfo` | 1.5 | mergeinfo (out of scope) | not requested |
| `SVN-VTxn-*` stubs | 1.8 | optional | `Option` fields stay `None` |

Tests for these paths (`tests/older_servers.rs`) use OPTIONS answers synthesized from the recorded
1.14.5 answer by removing tokens and headers. Replace them with recordings from real 1.8, 1.9
and 1.10 servers once the compat images exist; no behaviour was verified against such a server.

## Hard limits and encodings

* `ParserLimits` holds every cap in one place (defaults): href and status 8 KiB, one text node
  16 MiB, log message 16 MiB (raise it per parser), 10k elements per property value, 1024
  properties and 64 MiB of property text per `D:response`, 1M changed paths per log item, error
  message 4 KiB (control characters stripped, `…` marks the cut), error body 64 KiB. Exceeding a
  cap is `ErrorCode::Corrupt`. Text inside elements a parser does not know is never buffered.
* We always send `S:encode-binary-props`, so log fields (author, date, message, custom revprops)
  and get-locks fields can arrive with `encoding="base64"`. They are decoded (UTF-8 required for
  text fields); any other encoding value is `Corrupt`. Verified for a commit message containing
  ESC (`log/control-chars`). svn refuses control characters in lock comments, so lock fields are
  covered by synthetic tests only.
* The XML reader rejects what expat rejects: characters outside XML 1.0 `Char` (also as
  `&#1;`), `<` inside attribute values, a declaration that is not first.
* Lock tokens are read only from `activelock/locktoken/href` and must be
  `opaquelocktoken:` + printable characters (no whitespace, controls, `<>`, max 256 bytes).
* `REPOS_HOOK_FAILURE` (165001) maps to `Internal` until the contract adds a hook-failure code.

## Contract notes

* Log and lock paths are `RepoDisplayPath` values (private field, no `Deref`, no `AsRef<Path>`,
  no conversion to `RelPath`): structurally validated only, so a repo containing `aux.c` can still
  be logged. `LogItem::changed_paths` and `RepoLock` carry them; `LogItem::entry.changed_paths` is
  left empty on purpose. `into_log_entry()` / `into_lock_info()` produce the shared contract types,
  whose `path` fields are plain `String`s: call them only at that boundary. A file system path
  requires an explicit `RelPath::from_server(display.as_str().trim_start_matches('/'))`.
  Node names that may become working-copy paths (`DirEntry.name`, `accept_href`) always go
  through `RelPath::from_server`.
* A listing never drops entries silently: names that fail validation come back as
  `RejectedEntry` next to the accepted ones; the caller decides whether that is fatal.
* `UrlBuilder::accept_href` accepts only: same scheme/host/port as the session and a path equal to
  or below one of the announced stubs or the repository root; no query/fragment, no `.`/`..`
  (also percent-encoded), no `\`, no control characters, no encoded `/`, no unknown resource
  below the directory that holds the stubs.

## Not verified yet

* Behaviour of a Subversion **1.15 server** (needs P06 image) and of **VisualSVN**.
* `Timeout` header semantics on LOCK, HTTP status for a failed `If:` precondition (assumed 412 ->
  `LockTokenInvalid`), `Negotiate` challenge headers.
* `strict-node-history` element position; `mergeinfo-report` and `get-location-segments` bodies.
* Everything about servers older than 1.14 (see the capability table).
* Server GC of abandoned transactions.
