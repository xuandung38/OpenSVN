# svn-protocol notes: what was verified against real traffic

Scope of this file: facts about the Subversion HTTPv2 (mod_dav_svn) wire protocol that the
`svn-protocol` goldens depend on. "Verified" means observed in both recording sets:

| Set | Server | Client |
|---|---|---|
| `tests/data/` | Apache 2.4.68 (Debian) + mod_dav_svn **1.14.5** | svn 1.15.0 / serf 1.3.10 |
| `tests/data-1.15/` | Apache 2.4.69 + mod_dav_svn **1.15.0** (`docker/nix/subversion-1.15.nix`) | svn 1.15.0 / serf 1.3.10 |
| `fixtures/protocol/` | Apache 2.4.58 (Ubuntu) + mod_dav_svn **1.14.3** | svn 1.14.3 |
| `fixtures/protocol-1.15/` | Apache 2.4.69 + mod_dav_svn **1.15.0** | svn 1.15.0 / serf 1.3.10 |

Every `*_golden.rs` test runs once per `tests/` set (`common::for_each_set`), and
`svn-ra/tests/fixture_replay.rs` replays every session of both `fixtures/` sets. Everything else is
marked unverified. Behaviour of VisualSVN is not yet recorded.

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

The 1.15 sets were recorded without docker, from a native Apache and the nix build of 1.15.0:

```
nix-build docker/nix/subversion-1.15.nix --arg httpServer true -o target/svn-1.15-http
nix-build '<nixpkgs>' -A apacheHttpd -o target/httpd
export PATH=$PWD/target/svn-1.15-http/bin:$PWD/target/httpd/bin:$PATH
export CAPTURE_HTTPD=$PWD/target/httpd/bin/httpd CAPTURE_HTTPD_MODULES=$PWD/target/httpd/modules \
       CAPTURE_SVN_MODULES=$PWD/target/svn-1.15-http/modules
crates/svn-protocol/tools/capture/run.sh /tmp/raw-115
python3 crates/svn-protocol/tools/capture/extract.py /tmp/raw-115 crates/svn-protocol/tests/data-1.15
CAPTURE_OUT=fixtures/protocol-1.15 fixtures/protocol/capture.sh
fixtures/check-no-real-hosts.sh                            # also scans tests/data and tests/data-1.15
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
* `run.sh` is one scenario, so both `tests/` sets hold the same exchanges at the same revision
  numbers and the golden assertions (`size == 17`, `copyfrom-rev == 4`, ...) hold for both.
  `commit-control-chars` used to edit `src/b.txt`, which `commit-full` deletes; it now edits
  `src/deep/c.cfg` (r12 in `data-1.15`). The `data/` set predates that fix.

### Fuzzing

Targets live in `fuzz/fuzz_targets/` (standalone crate, needs a nightly toolchain):

```
cd crates/svn-protocol/fuzz
cargo +nightly fuzz build                 # compile all targets
cargo +nightly fuzz run multistatus -- -max_total_time=3600
```

The 1h-per-target run belongs to `.github/workflows/fuzz.yml` (nightly and manual). It runs every
file in `fuzz/fuzz_targets/` here and in `crates/svn-delta/fuzz/`, and uploads `artifacts/<target>/`
as `fuzz-crashes-<target>`. Both fuzz crates commit their `Cargo.lock`. The stable test suite covers
the same entry points with `tests/mutation_smoke.rs`. It also replays every past crash input from
`tests/data/fuzz-regressions/<target>/` with `tests/fuzz_regressions.rs`.

#### Local run, 2026-10-15

Setup: `cargo +nightly fuzz run <target> corpus/<target> -- -max_total_time=960`, using
cargo-fuzz 0.13.2 and nightly rustc. Four targets ran at a time on a 4-core container, so exec/s
is lower than on an idle machine. Each svn-protocol target except `href_vetting` started from the
recorded `tests/data/**/*.resp` bodies. `cov` is libFuzzer's edge coverage at the end of the run.

| Target | Time | Execs | exec/s | cov | ft | Corpus | Crash |
|---|---|---|---|---|---|---|---|
| svn-protocol `error_body` | 16 min | 7,978,997 | 8,302 | 1,849 | 8,692 | 1,854 | none |
| svn-protocol `file_revs` (before the fix) | 11 min | 2,065,109 | 3,148 | 2,940 | 12,425 | 2,063 | **panic** (see below) |
| svn-protocol `file_revs` (after the fix) | 16 min | 5,083,217 | 5,289 | 3,287 | 15,245 | 2,424 | none |
| svn-protocol `get_locks` | 16 min | 8,300,395 | 8,637 | 2,019 | 8,776 | 1,751 | none |
| svn-protocol `href_vetting` | 16 min | 2,433,734 | 2,532 | 3,416 | 12,078 | 2,436 | none |
| svn-protocol `log_parser` | 16 min | 10,752,665 | 11,189 | 2,485 | 10,990 | 2,109 | none |
| svn-protocol `multistatus` | 16 min | 12,004,190 | 12,491 | 2,065 | 10,266 | 2,087 | none |
| svn-protocol `update_parser` | 16 min | 1,483,296 | 1,543 | 4,379 | 15,205 | 1,798 | none |
| svn-protocol `update_parser` (after the fix) | 16 min | 1,627,887 | 1,693 | 4,644 | 16,858 | 1,926 | none |
| svn-protocol `xml_reader` | 16 min | 7,245,789 | 7,539 | 1,528 | 8,184 | 1,512 | none |
| svn-delta `decode` | 16 min | 28,507,905 | 29,664 | 1,135 | 3,977 | 825 | none |
| svn-delta `roundtrip` | 16 min | 5,158,568 | 5,367 | 1,547 | 5,117 | 880 | none |

**Crash found and fixed:** `file_revs` panicked with `assertion failed: self.is_char_boundary(at)`
in `util::Base64Stream::push`. The stream kept every non-whitespace character and split its
buffer at a multiple of 4 *bytes*. A multi-byte character inside `S:txdelta` (the input had a
U+FEFF) put that split point inside the character, and `String::split_off` panicked. The same
stream also decodes `S:txdelta` in the update-report parser. The base64 alphabet is ASCII, so a
non-ASCII character is now a corrupt response (`invalid base64: non-ASCII character`), which
keeps the buffer at one byte per character. The input is
`tests/data/fuzz-regressions/file_revs/crash-bb93c394…`. Regression tests: unit test
`util::tests::base64_stream_rejects_non_ascii_instead_of_splitting_a_char`, and
`tests/fuzz_regressions.rs`, which replays the artifact and checks every split point reports the
response as corrupt. Both reruns marked "after the fix" ran on the fixed code.

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

### update-report (verified with svn 1.15.0 against 1.14.5 and 1.15.0)

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

## mod_dav_svn 1.15.0 compared with 1.14

Recorded with the same client (svn 1.15.0) and the same scenario, then compared after masking
UUIDs, timestamps, lock tokens and the `User-Agent`. No parser change was needed.

* **OPTIONS:** the same headers, the same `DAV:` capability tokens (no new 1.15 token), the same
  `SVN-Allow-Bulk-Updates`, `SVN-Supported-Posts` and stub headers, and the same body.
* **Every other answer** (PROPFIND, GET, log/get-locks/list/file-revs/update reports, LOCK/UNLOCK,
  POST/MKCOL/PUT/COPY/PROPPATCH/DELETE/MERGE, 401/404/409/423 errors) carries the same elements and
  values. The only differences are orders the parsers already must not depend on:
  * element order inside `S:log-item` (`D:creator-displayname`, `S:date`, `D:comment`) and of
    `S:rev-prop` inside `S:file-rev`;
  * `S:set-prop` order inside an update drive (property hash order).
* Differences that come from Apache or its configuration, not from mod_dav_svn: the 401/201 HTML
  pages of Apache 2.4.69 use an HTML 4.01 doctype and carry no `<address>` signature line, and
  `allprop` has no `D:getcontenttype` when `mod_mime` is not loaded (the native capture server
  loads only the modules it needs).
* Client-side orders (`S:entry` attribute and entry order, `create-txn-with-props` property order)
  differ between runs as before; the request tests compare them as XML or as skel lists.

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
* `REPOS_HOOK_FAILURE` (165001) maps to `HookFailed`; 301/302/307/308 and errcode 175011 map to `RepositoryMoved` (the validated `Location` is in the message).

## Update drive order (parser is the gate)

`UpdateParser` accepts exactly one anonymous root `open-directory`, entries only below
directories, `txdelta`/`fetch-file` only below a file and at most once, property changes only
below a node, and nothing after the root closes; end tags must match the open element by
namespace and name. Every violation is `Corrupt`. `DriveChecker` re-checks a finished event
stream (usable as a debug assertion in the driver and as the fuzz oracle).

* The first event is `ReportStart { send_all, inline_props }`. In skelta (`send_all` false) a
  file whose text was not sent gets a synthesized `FetchFile` before its `CloseFile`, so an added
  file is never silently empty; without `inline_props` added nodes get `FetchProps` the same way.
* `AddDir`/`AddFile` carry `copy_from` (raw server text; with a copy source the text delta is
  against the source's content) and `DeleteEntry` carries `rev`.
* A name that `RelPath::from_server` rejects (`..`, `.svn`, `CON`, `a:b` ...) stops the whole
  update with `UnsafePath` and a message naming the offending path. Repositories containing such
  names cannot be checked out in the MVP.
* `PutOptions`/`put_request` take a `Body`: pass `Body::Stream(ReadBody(spill_file))` for large
  svndiff data. Transactions are `TxnName` (plain or virtual); `SVN-VTxn-Name` answers use the
  `vtxn`/`vtxr` stubs. Directory `COPY` sends `Depth: infinity`.
* svndiff version: serf's preference, 1 unless `low_latency` and svndiff2 is announced.
* `UpdateMode::select` asks for send-all like serf when a server sends neither
  `SVN-Allow-Bulk-Updates` nor `inline-props` (skelta would cost a request per node).

## Contract notes

* Log and lock paths are `svn_types::RepoDisplayPath` values (private field, no `Deref`, no
  `AsRef<Path>`, no conversion to `RelPath`), used directly by `ChangedPath` and `LockInfo`:
  structurally validated only, so a repo containing `aux.c` or `a:b` can still be logged. A file
  system path requires an explicit `RelPath::from_server(display.as_str().trim_start_matches('/'))`.
  Node names that may become working-copy paths (`DirEntry.name`, `accept_href`) always go
  through `RelPath::from_server`.
* A listing never drops entries silently: names that fail validation come back as
  `RejectedEntry` next to the accepted ones; the caller decides whether that is fatal.
* `UrlBuilder::accept_href` accepts only: same scheme/host/port as the session and a path equal to
  or below one of the announced stubs or the repository root; no query/fragment, no `.`/`..`
  (also percent-encoded), no `\`, no control characters, no encoded `/`, no unknown resource
  below the directory that holds the stubs.

## Not verified yet

* Behaviour of **VisualSVN** (only mod_dav_svn 1.14.3, 1.14.5 and 1.15.0 are recorded).
* `Timeout` header semantics on LOCK, HTTP status for a failed `If:` precondition (assumed 412 ->
  `LockTokenInvalid`), `Negotiate` challenge headers.
* `strict-node-history` element position; `mergeinfo-report` and `get-location-segments` bodies.
* Everything about servers older than 1.14 (see the capability table).
* Server GC of abandoned transactions.
