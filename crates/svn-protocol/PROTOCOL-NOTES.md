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

### Seen while capturing, needed for later work
* update-report: request on `!svn/me`, `send-all="true"` when the server says
  `SVN-Allow-Bulk-Updates: Prefer`; response root has `inline-props="true"`; `add-file` carries
  `sha1-checksum`, `add-directory` carries `bc-url` and `checked-in` hrefs are `!svn/rvr/<rev>/..`.
* commit: `POST !svn/me` with `application/vnd.svn-skel` (`create-txn-with-props`), reply header
  `SVN-Txn-Name`; `PUT !svn/txr/<txn>/<path>` with `Content-Type: application/vnd.svn-svndiff`,
  `X-SVN-Version-Name`, `X-SVN-Base-Fulltext-MD5`; success echoes `X-SVN-Result-Fulltext-MD5`;
  **`MERGE` goes to the public URL of the session path** (`/svn/repo/trunk`), body names the txn
  in `D:source/D:href`, header `X-SVN-Options: release-locks no-merge-response`; with
  `no-merge-response` the answer only carries the new revision under `!svn/vcc/default`.
* A stale PUT is answered 409 and the client then `DELETE`s the transaction.

## Contract notes

* `ChangedPath.path` and `LockInfo.path` are display text. They are validated structurally
  (`RelPath::new`: absolute, no `.`/`..`/empty segment, no NUL) and not with `from_server`, so a
  repository containing `aux.c` can still be logged. Node names that may become working-copy
  paths (`DirEntry.name`, anything from `accept_href`) always go through `RelPath::from_server`.
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
* `strict-node-history` element position; `file-revs-report`, `mergeinfo-report`, `list-report`,
  `get-location-segments` bodies.
* Server GC of abandoned transactions.
