# Hostile-server fixtures

What a malicious or broken server could send, and what the engine must do with it. Every case is
one file plus one row in `cases.tsv` (`id`, `parser`, `file`, `expect`, `description`); the
description column is the per-case attack write-up. Regenerate with
`python3 fixtures/evil/generate.py` (deterministic, stdlib only).

Documents assume the session `http(s)://svn.test.local/svn/repo` (repository root `/svn/repo`).

## Categories

| Dir | Fed to | Attacks |
|---|---|---|
| `update/` | `svn_protocol::update::parse_update` / `UpdateParser` | node names `../x`, `..`, `.`, `/etc/passwd`, `sub/../../x`, `C:\…`, `C:evil`, `\\server\share`, `..\..\`, `.svn` + `wc.db`, `.SVN`, `CON.txt`, `a:b` (ADS), trailing dot/space, `delete-entry ../victim`; `checked-in` hrefs on another host, with `..` / `%2e%2e`, outside the repository, ending in `.. ` (the URL parser strips the space) or with `..` before a `://` inside the path; truncated, mismatched tags, billion laughs, XXE, invalid UTF-8, `&#0;`, overflowing/negative revisions, binary garbage |
| `propfind/` | `svn_protocol::propfind::parse_list` / `ListParser` | child hrefs with `..`, `%2e%2e`, `%2f`, `%5c`, `%00`, `.svn`, another host, outside the listed directory, a trailing `.. `, `..` before a `://` inside the path |
| `log/` | `svn_protocol::log::parse_log` | changed paths / copyfrom with `..` or not absolute, bad revision, truncation; an HTML/script commit message |
| `merge/` | `svn_protocol::commit::parse_merge_response` | negative or non-numeric new revision |
| `locks/` | `svn_protocol::locks::parse_get_locks` | lock paths with `..` or not absolute |
| `options/` | `Capabilities::from_options_response` + `UrlBuilder::new` | repository root / `me` / stubs on another host or protocol-relative, root outside the session, root with `..` (also after `\`), any `\`, tab or other control character and their percent escapes (`%09`, `%5C`, ...) in a root or stub, `..` hidden before a `://` inside the path, overflowing youngest revision |
| `svndiff/` | `svn_delta::SvndiffDecoder` (+ `WindowApplier` for `svndiff-apply`, + `Base64SvndiffDecoder`) | bad magic, unknown version, truncation, varint overflow, 1 TiB target view / new data, source view overflow, reads past new data / source view / unwritten target, short/long target, reserved opcode, zlib bomb (claims 5 B, inflates to 1 MiB), bad zlib/lz4, source beyond the base text |

Transport-level attacks have no data file; `crates/svn-ra/tests/evil_transport.rs` serves them
from a scripted HTTPS server: redirects to another port, host or scheme (`http:` downgrade),
protocol-relative and backslash `Location`s, `user:pw@` and `host@other` tricks, `file:` and
`javascript:` targets, all for GET/PROPFIND/REPORT/OPTIONS; a 1 GiB body against a 1 MiB limit;
a stalled body (read timeout); certificates for another name or a wildcard (hard failure, never
promptable); and every `update/` case delivered over the wire.

## Expectations (`expect` column)

- `unsafe` / `corrupt`: the parser returns an error with `ErrorCode::UnsafePath` or
  `ErrorCode::Corrupt` (either is a safe rejection), whole and when fed one byte at a time.
- `quarantine`: a listing may still succeed, but the hostile child is reported only as
  `ListItem::Rejected`, never as an entry.
- `ok-verbatim`: accepted and returned as plain text; escaping is the UI's job.
- `reject`: OPTIONS answer refused by `Capabilities` or `UrlBuilder`.

## Known gaps / not covered

- No `docker/evil-server/` container yet: the scripted servers in the crate tests cover the same
  answers without Docker. A hostile *repository* (file names / props containing `<script>`, SVG
  with script, symlink `dir -> /tmp`) for UI and working-copy tests is not generated yet.

## Hostile repository and evil server

`make-hostile-repo.sh <repo-path>` builds a real Subversion repository that attacks a client UI
and its working copy. `docker/evil-server/run-local.sh up` builds it and serves it with Apache +
mod_dav_svn over HTTPS at `https://localhost:18543/svn/hostile` (Basic `tester` / `tester-pass`,
throwaway CA under `$EVIL_DIR/certs`), without Docker; `docker/evil-server/Dockerfile` runs the
same script in a container. `apps/opensvn/e2e/hostile.test.mjs` drives the real app against it
(see `apps/opensvn/e2e/README.md`).

Every network beacon points at `$BEACON` (default `https://beacon.evil.test/`; the e2e test uses a
local listener that must receive nothing). The repository is deterministic for a given `BEACON`
(fixed UUID and dates; the builder prints the dump SHA-256). It uses only `*.test` hosts.

| Where | Case | Attack | What a client must do |
|---|---|---|---|
| r1 log | `Layout <script>alert('log r1')</script>` | script element in a log message | show as text |
| r2 log, r2 author | `<img src=x onerror=…>` | event handler in a log message and in `svn:author` | show as text |
| r3 log | `[click me](javascript:…)`, `<a href="javascript:…">`, `$BEACON/log.png` | markdown/HTML link to `javascript:`, a beacon URL | no `javascript:` link, no fetch |
| r3 author, r9 author | `<script>…</script>`, `<svg onload=…>` | markup in authors | show as text |
| r4 log | ESC `[31m` … `[0m`, U+202E … U+202C | ANSI escapes and a bidi override in a log message | show as text, no terminal/bidi effect on the UI |
| r6 log | 256 KiB message | absurdly long log message | render without hanging |
| r9 log | `\xff\xfe\xc3\x28`, CR LF, BEL, `ESC[2J`, `<script>` | invalid UTF-8 and control bytes in `svn:log` (only via `svnadmin load --bypass-prop-validation`; mod_dav_svn sends it base64) | show with U+FFFD, as svn shows it escaped; one bad revision must not hide the rest of the log |
| `trunk/{{constructor.constructor('alert(1)')()}}.txt`, `trunk/${alert('name')}.txt`, `trunk/'-alert('name')-'.txt` | template / string-break injection in names | show as text |
| `trunk/&lt;img src=x onerror=alert('entity')&gt;.txt` | entity-encoded markup in a name (double-decoding) | show the literal `&lt;` |
| `trunk/%3Cscript%3E…%3C%2Fscript%3E.txt` | percent-encoded markup in a name (double URL-decoding; `%2F` would become `/`) | keep the literal `%`; request it as `%25…` |
| `trunk/invoice‮txt.exe` | bidi override hides the real extension | show the override visibly (escaped) |
| `trunk/evil.svg` (`image/svg+xml`) | `<script>`, `onload`, `<image href=$BEACON…>`, `<foreignObject><iframe src=javascript:>`, `<a href=javascript:>` | never render as an image/document; text preview only |
| `trunk/evil.html` (`text/html`) | `<script>`, `<meta refresh=javascript:>`, stylesheet/image/iframe beacons | text preview only |
| `trunk/xss-content.txt` (`text/html` on a `.txt`) | content type that lies; markup in content | text preview only |
| `trunk/evil.md` | markdown `javascript:` link, image beacon, `<details ontoggle>` | never rendered as markdown |
| `trunk`, `trunk/props.txt`, `trunk/evil.svg`, `names/<img…>.txt` props | `custom:xss` `<script>`, `custom:img` `<img onerror>`, `custom:link` `javascript:`, `custom:beacon` `<img src=$BEACON…>`, `custom:huge` 48 KiB of `<b>` | markup and beacons in property values | show as text, no fetch |
| `trunk/props.txt` `custom:bad-utf8` | `\xff\xfe<script>…\x00\x01` | binary property value (svn stores custom values as given; served base64) | show as text, no crash |
| `trunk` `svn:ignore` | `<script>alert('ignore')</script>` | markup in an `svn:` property | show as text |
| `trunk/escape-up` → `../../../../../../../../../../../../etc/passwd` | relative symlink out of the working copy | create it as svn does (it is data), never follow it: previews/diffs show `link <target>`; open, reveal, editor and terminal refuse it |
| `trunk/escape-abs` → `/etc/passwd`, `trunk/escape-dir` → `/etc` | absolute symlinks to a file and a directory | as above; `escape-dir/passwd` is refused as a path through a symlinked folder |
| `trunk/escape-parent` → `../..` | symlink to an ancestor of the working copy | as above |
| `trunk/escape-loop` → itself | symlink loop | no hang |
| `trunk/into-admin` → `.svn/wc.db` | symlink into the administrative area | never followed (no write through it) |
| `trunk/LLL….txt` (255 bytes), `trunk/deep/<8 × 200-byte dirs>/leaf.txt` (~1.6 KiB path) | longest name Linux allows, very deep path | check out or fail cleanly |
| `trunk/bad-utf8.txt` (r9) | invalid UTF-8 content, `custom:bad-utf8` = `\xc0\xaf\xed\xa0\x80` (overlong `/`, surrogate) | binary-safe content and properties |
| `names/<img src=x onerror=alert('name')>.txt`, `names/<script>alert('name')<%2Fscript>.txt`, `names/"><iframe src=javascript:…>.txt`, `names/<svg onload=alert('dir')>/inner.txt` | raw markup in file and directory names | show as text wherever named (log changed paths); refuse to write them to disk |
| `names/javascript:alert('name').txt`, `names/back\slash.txt`, `names/pipe|star*question?.txt`, `names/ leading and trailing space `, `names/trailing dot.`, `names/CON.txt` | names invalid or dangerous on Windows (`:` stream separator, `\`, `<>"|?*`, trailing dot/space, device name) | refuse to create (OpenSVN refuses on every OS: `RelPath::from_server`) |
| `branches/too-long/N…` (300 bytes), `branches/too-long/M…` (1000 bytes) | names over NAME_MAX | fail cleanly, never truncate into another name |
| `branches/<svg onload=alert('branch')>`, `tags/<img src=x onerror=alert('tag')>`, r8 log `<script>alert('branch log')</script>` | markup in branch and tag names (Branch menu) | show as text |
| `branches/externals` `svn:externals` = `$BEACON/<script>alert('ext')<%2Fscript> ext` | external pointing at a hostile URL | not fetched unless the user checks that branch out; never rendered as markup |

Not in the repository, because svn itself does not allow it:

- **Invalid UTF-8 in a path.** The filesystem refuses it (`E160005`), even through
  `svnadmin load --bypass-prop-validation`.
- **Invalid UTF-8 in `svn:author`.** It loads, but mod_dav_svn writes `svn:author` raw into
  PROPFIND answers (`creator-displayname`), so the XML is malformed and svn 1.15's own `svn ls -v`
  fails on it (`E130003`). A client must report that as a server error (OpenSVN does).
- **Property names with markup.** svn restricts property names to XML name characters.
