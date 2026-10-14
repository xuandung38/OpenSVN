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
