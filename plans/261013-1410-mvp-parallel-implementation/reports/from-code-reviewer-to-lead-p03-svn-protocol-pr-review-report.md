# P03 svn-protocol M1 — PR #40 review (code-reviewer → lead)

- PR: xuandung38/OpenSVN#40 `feat: add svn-protocol dav core, propfind, log and lock`
- Reviewed SHA: **a5e8631** (head moved from 46bd535 during review; I re-ran the probes on a5e8631)
- Scope: `crates/svn-protocol/**` (src ~3.8k LOC, tests, fixtures, fuzz, tools/capture), `tests/compat/scenarios/protocol/*.yaml`, `Cargo.lock` (regenerated only)
- Threat model: hostile or compromised server/proxy on the session origin.

## Verdict: REQUEST CHANGES

Two IMPORTANT defects, both reproduced with probe tests: (1) values the server base64-encodes are never decoded, which breaks log and get-locks output for real repositories; (2) several parser buffers have no size limit, so a hostile server can exhaust client memory, contrary to the phase NFR. Everything else is MODERATE. The XML hardening, href vetting, sans-IO purity and fixtures are solid.

## Gates

| Gate | Result |
|---|---|
| `cargo fmt --all --check` | ok (local, a5e8631) |
| `cargo clippy -p svn-protocol --all-targets --all-features --locked -D warnings` | ok (local) |
| `cargo test -p svn-protocol --locked` | 101 passed, 0 failed (local) |
| `cargo deny check` | advisories/bans/licenses/sources ok (local, 46bd535; deps unchanged since) |
| CI run 38377964681 @ a5e8631 | **all green**: rust macos/ubuntu/**windows-2025**, cargo-deny, tauri x3, web x3, bindings |
| Workspace clippy/test | covered by CI rust jobs (green on 3 OS) |
| unwrap/expect/panic/unsafe in lib | none; all occurrences are in `#[cfg(test)]` |
| Owned paths | ok: `crates/svn-protocol/**`, `tests/compat/scenarios/protocol/**` (both in the P03 `owner_paths`); `Cargo.lock` regen only (CODEOWNERS lead) |
| Compat (Docker) job, required for P03 per plan rule 4 | **absent**: the P06 workflow does not exist yet. Process gap, not a code defect |
| Fuzz 1h/target | not run (needs nightly); the PR says so |

## Verified OK (evidence)

- **DTD/entities**: the scanner rejects `<!D…` before quick-xml sees it, even when split across chunks (probe: `feed("<!")`, then `feed("DOCTYPE…")` → error). Only the 5 predefined entities and char refs are accepted (`xml/reader.rs:358-375`, `:240`).
- **Limits**: max_markup 1 MiB, depth 128, attrs 128, text flushed every 8 KiB at a UTF-8/entity-safe cut (`reader.rs:30-39, 282, 292-311`).
- **Namespaces resolved by URI, not prefix**: confirmed by the real fixture `locks/lock-ok.resp`, where the server sends `<ns0:owner xmlns:ns0="DAV:">`. Custom props `C:custom:tag` keep `custom:tag` as the local name.
- **Parser stays failed after an error**: XmlReader plus every ResponseParser (`failed` flag).
- **Href vetting** (`caps/url_builder.rs:221-263`): probe results match expectations. Rejected: `%2e%2e`, `%2F`, `.svn`/`%2esvn`, `aux.c` (via `from_server`), a different scheme, userinfo, `//net`, relative hrefs, overflowing revision numbers. Accepted: `http://host:80` as the same origin. Hrefs are percent-decoded and then passed through `RelPath::from_server` (`decode_rel`, `:353-366`).
- **Display-only paths**: after e8fb84f/a5e8631 they are `RepoDisplayPath` (private field, no Deref/AsRef<Path>, no conversion to `RelPath`). They cannot reach `safe_join(&Path, &RelPath)` without an explicit `RelPath::from_server`. `/a:b` and `aux.c` now log fine (probe P1 ok).
- **Sans-IO**: dependencies are base64, bytes, quick-xml, svn-types and url only; `tests/no_io_dependencies.rs` enforces this.
- **Error codes**: I checked every constant in `errors.rs:20-89` against the `svn_error_codes.h` category/offset order; all correct.
- **Wire parity**: `stat-dir.req` matches the serf PROPFIND body, and the log body order matches serf `create_log_body` (start, end, limit, flags, revprops, `S:path`, `encode-binary-props`). The `wire_parity` test compares method, path, body and every non-transport header.
- **Fixtures**: no Authorization, Date, Server or Cookie headers; `127.0.0.1` is rewritten to `svn.test.local`; no `/Users`, `/var/folders` or `/tmp` paths. Leftovers are harmless: `Port 18081` in Apache HTML footers and the User-Agent OS string. `.gitattributes -text` keeps fixtures byte-exact on Windows.

## IMPORTANT

### I1. `encoding="base64"` ignored in log-report and get-locks-report: wrong data for real repos
- `log/request.rs:109` always sends `<S:encode-binary-props/>`. Because of that, mod_dav_svn (`reports/log.c:271-306`, 1.14.x) base64-encodes `D:comment`, `D:creator-displayname`, `S:date` and `S:revprop` whenever a value is not XML-safe (any C0 control other than TAB/LF/CR, e.g. ESC from pasted terminal output).
- `log/parser.rs:178,180` stores the raw text. Probe: `<D:comment encoding="base64">aGkbWzBt</D:comment>` → `message = "aGkbWzBt"` (the base64 text itself). A base64 `S:date` would make `parse_timestamp` fail and kill the whole stream.
- Same bug in get-locks: `reports/get-locks.c:97-145` base64-encodes `S:owner`/`S:comment`, and `locks/get_locks.rs:110-111` stores them raw. Probe: owner `"Ym9iAQ=="`, comment `"aGkbWzBt"`.
- Fix: in both parsers, record the `encoding` attribute at field start. On close, decode `base64` (ignoring whitespace, as in `propfind/multistatus.rs:209-219`), then require valid UTF-8 for author/message/owner/comment. Any other encoding value → `Corrupt`, like serf `log.c:160-170`. Add fixtures by capturing a commit with `-F` from a message containing `\x1b`, and a lock with such a comment.

### I2. Unbounded buffers: a hostile server can OOM the client (violates the "memory independent of response size" NFR)
| Location | Buffer | Probe |
|---|---|---|
| `propfind/multistatus.rs:189` | `entry.href` | 64 MiB of href text accepted |
| `propfind/multistatus.rs:191` | `status_text` | same pattern |
| `propfind/multistatus.rs:143,200` | `child_names`/`descendants`: empty children add nothing to `size` | 4,194,304 `<a/>` in one prop accepted (~15x amplification) |
| `propfind/multistatus.rs:237` | props per `D:response` (per-prop cap 8 MiB, no count or total cap) | n × 8 MiB |
| `log/parser.rs:240` | `text` (message/author/path), plus `paths` per item | code read |
| `locks/get_locks.rs:84` | `text` for **any** `S:*` field, including unknown ones | 32 MiB in `<S:junk>` accepted |
| `errors.rs:171` | message up to the 1 MiB body | 900 KB message returned |
- Fix: add one `ParserLimits` (or per-module consts): href ≤ 64 KiB, status ≤ 1 KiB, text field ≤ 16 MiB (log message), descendants/child_names ≤ 1024 per prop, ≤ 64 MiB total per entry. Do not buffer text for unknown fields (`get_locks.rs:73-76` should only set `field` for known names). Return `Corrupt` when a limit is exceeded. Add a test per limit, like `xml_reader.rs::limits_bound_memory`.

## MODERATE

### M1. Lock token taken from any `D:href` under `activelock`
- `locks/response.rs:74,80` concatenates every `D:href` text, and `:122` picks the first `href` descendant. A lock with a DAV-style comment (mod_dav_svn `lock.c:84-100` stores `is_dav_comment` XML verbatim, e.g. `<D:owner><D:href>mailto:…</D:href></D:owner>`) breaks this. Probe: LOCK token `"mailto:bob@svn.test.localopaquelocktoken:1234"`; get-lock token `"mailto:bob"`. Unlock/commit with that lock then fails.
- `token_from_href` (`:53-59`) only trims. A token containing `\n` is accepted (probe: `"opaquelocktoken:1\nX-Evil: 1"`). `unlock_request` validates its token, but the M2 `If:`/`S:lock-token-list` builders will read `RepoLock.token`.
- Fix: match the `activelock/locktoken/href` path only (serf `lock.c:124-128`). Validate the token at parse time: no control chars or whitespace, no `<>`; preferably require the `opaquelocktoken:` prefix.

### M2. Error text is passed to the UI unbounded and unsanitised
- `errors.rs:14,171`: probe shows a 900 KB message containing ESC kept verbatim. The UI renders text only, but huge or control-laden strings still reach toasts, the activity log and logs.
- Fix: cap each message (e.g. 4 KiB, adding `…`), cap the chain total, strip C0 controls except `\n\t`, and lower `MAX_ERROR_BODY` to 64 KiB.

### M3. Error-code semantics
- `errors.rs:112`: `REPOS_HOOK_FAILURE` (165001) → `Forbidden`. Pre-commit, pre-lock and pre-revprop-change rejections are policy messages, not authz failures, so the UI would show "access denied" or offer a re-login. Map to `Internal` and keep `svn_code` (or propose a `HookRejected` variant in a `contract:` PR).
- `errors.rs:130`: a 409 without a `D:error` → `OutOfDate`. In WebDAV, 409 is also "parent collection missing" (MKCOL/PUT/COPY). That is acceptable as a fallback, but add a comment.
- Not a defect: 412 → `LockTokenInvalid` and 423 → `LockedByOther` match the captures.

### M4. `accept_href` classifies on the still-encoded path, so the special-resource guard can be bypassed
- `url_builder.rs:265-323` compares `url.path()` (encoded) with the stubs. Probe: `/svn/repo/%21svn/ver/3/trunk/a.txt` → `Public { path: "!svn/ver/3/trunk/a.txt" }`, while the `!svn` form is rejected. There is no FS escape (`from_server` still applies), but the guard promised in PROTOCOL-NOTES ("everything under the stub directory that is not a known stub is unsafe") does not hold.
- Fix: percent-decode the path segments once (rejecting `%2F`), then compare against decoded stubs; or reject any `%xx` that decodes to an unreserved/sub-delim character. Add the probe case to the golden tests and the fuzz oracle.

### M5. Display-only typing ends at the RA contract boundary
- `display_path.rs:63,94`: `into_contract()`/`into_lock_info()` turn the path back into a plain `String`, because `svn_types::{ChangedPath, LockInfo}.path: String`. P10/P15 will call these immediately, so the protection lasts only inside this crate.
- Recommend a `contract:` PR moving `RepoDisplayPath` into `svn-types`, used by `ChangedPath.path`, `copy_from` and `LockInfo.path`.
- Also: `LogItem.entry.changed_paths` is always empty while `LogItem.changed_paths` holds the data. Two sources of truth invite misuse; remove the field from the public shape or document it on `LogItem`.

### M6. Namespace-insensitive attribute lookup
- `xml/reader.rs:61-66` `StartTag::attr` matches on local name only, in any namespace, first match wins. It is used for `encoding`, `errcode`, `copyfrom-*`, `node-kind`. Low exploitability (the server controls the content anyway), but the task asked for exact handling.
- Fix: add `attr_ns(&Ns, local)`, use `Ns::None` for svn report attributes and `Ns::None | Ns::SvnDav` for the PROPFIND `V:encoding`.

### M7. XML reader is laxer than expat (which svn uses)
- Probes accepted `&#1;`, a raw `\x01` byte, `<` inside an attribute value, and a `<?xml?>` declaration after the root start. None of these is exploitable for paths (`from_server` and `validate_display_path` reject controls), but C0 controls do flow into author, message and owner.
- Fix: reject non-XML-1.0 `Char` in text and attributes, as `XmlWriter::is_xml_char` already does on output.

### M8. `LogRevprops::All`/`Only` request custom revprops that are then dropped
- `log/request.rs:91-100` asks for them; the parser ignores `S:revprop` and `LogEntry` has no field for them. Either remove these variants for now or add `revprops` through a contract PR. Today a caller silently gets nothing.

### M9. Capture tooling binds the throwaway server on all interfaces with known credentials
- `tools/capture/run.sh:11` uses `-p 18080:80`. Use `-p 127.0.0.1:18080:80`. The alice/secret and bob/hunter2 test credentials are acceptable: throwaway container, documented.

## Low / informational
- `entries.rs:16-24`: the `RevRoot` revision in hrefs is not compared with the requested peg, and `parse_stat` does not check that the href equals the requested path. Defence in depth only.
- `tests/no_io_dependencies.rs`: the `"(/"` workspace-path heuristic never matches on Windows (`(C:\…)`), so that half of the test is a no-op there. The banned-crate half still works.
- `proxy.py` is loopback-only. `extract.py` drops `Authorization` before anything is written.

## Plan / DoD follow-ups (do not edit plan; lead decides)
- Done for M1: xml, errors, caps + UrlBuilder, propfind, get, log, get-locks, lock/unlock builders and parsers, snapshots, wire parity, proptest chunk-split, fuzz targets (compile only), PROTOCOL-NOTES.
- Open: 1.15 server and VisualSVN goldens; 1h fuzz per target; compat Docker job (blocked on P06); update-report, commit and file-revs (M2).

## Recommended actions (priority)
1. I1: decode base64 in log and get-locks, with fixtures.
2. I2: limits on all text/href/status/descendant buffers, with tests.
3. M1: token from `locktoken/href` only, validated at parse time.
4. M2/M3/M4: error text caps, hook mapping, decoded href classification.
5. M5/M8: contract PRs (display path type in svn-types; revprops), or narrow the API.

## Unresolved questions
1. Product call: should `REPOS_HOOK_FAILURE` get its own `ErrorCode` (contract PR) or stay `Internal` with `svn_code`?
2. Should a Depth-1 listing reject a child whose name fails `from_server` (today it is `Rejected` and the listing continues) or fail the whole listing? Current behaviour seems right for browsing; please confirm.
3. Can P03 merge before the P06 compat workflow exists, given plan rule 4?

Status: DONE
Summary: REQUEST CHANGES on PR #40 @ a5e8631. CI is green on all 3 OS and the XML/href/sans-IO hardening is solid, but base64-encoded log and lock fields are never decoded (wrong data on real servers) and several parser buffers have no size limit (hostile-server OOM). Plus 9 moderate items, chiefly lock token extraction, error text caps and encoded-href classification.

---

## Re-review @ 82f7fc1 (2026-10-13)

Commit under review: `82f7fc1 fix(svn-protocol): decode base64 fields, cap parser buffers, harden lock tokens, error text and href classification`. CI is blocked by billing, so all gates were run locally on a `git archive` copy with `CARGO_BUILD_JOBS=2`: `cargo fmt --all --check` ok; `cargo clippy -p svn-protocol --all-targets --all-features --locked -D warnings` ok; `cargo test -p svn-protocol --locked` **118 passed / 0 failed**. The scratch copy was cleaned afterwards (`cargo clean`).

### Probe results (same probes as the first pass, plus follow-ups)
| Item | Before | Now | Status |
|---|---|---|---|
| I1 log base64 | message `"aGkbWzBt"` | `"hi\u{1b}[0m"`, author decoded, base64 `S:date` parses, `S:revprop` decoded into `LogItem.revprops`, unknown `encoding` → Corrupt | **fixed** (fixture `log/control-chars` captured) |
| I1 get-locks base64 | owner `"Ym9iAQ=="` | `"bob\u{1}"`, comment decoded | **fixed** |
| I2 href 64 MiB | accepted | fails at the first chunk (`max_href_bytes` 8 KiB) | **fixed** |
| I2 4M empty children | accepted | fails (`max_prop_elements` 10 000) | **fixed** |
| I2 get-locks unknown `S:junk` | buffered | no longer buffered (`FIELDS` whitelist, `get_locks.rs`); no error, no growth | **fixed** |
| I2 log message | unbounded | capped at 16 MiB (`max_log_message_bytes`) → Corrupt; changed paths capped at 1 M | **fixed** |
| M1 DAV-comment href | token `"mailto:…opaquelocktoken:1234"` | `opaquelocktoken:1234` for both LOCK and get-lock; token only inside owner → error; CR/LF → `LockTokenInvalid` | **fixed** (`TOKEN_PATH` = `activelock/locktoken/href`, `validate_lock_token`) |
| M2 error text | 900 KB + ESC | body cap 64 KiB, message cap 4 KiB plus `…`, C0 controls stripped | **fixed** |
| M3 hook failure | Forbidden | Internal, with TODO until contract PR #47 | **accepted** (per lead) |
| M4 `%21svn` | `Public("!svn/ver/…")` | rejected as unknown special resource (decoded classification) | **fixed** |
| M6 attr namespace | local-name only | `attr` = no-namespace only, `attr_in(ns, …)`; PROPFIND uses `V:encoding` or plain | **fixed** |
| M7 reader laxness | `&#1;`, raw `\x01`, `<` in attr, late `<?xml?>` accepted | all rejected (XML 1.0 `Char` check on text, CDATA, char refs and attributes) | **fixed** |
| M8 revprops dropped | dropped | `LogItem.revprops` | **fixed** |
| M9 capture port | `0.0.0.0` | `127.0.0.1:18080` | **fixed** |
| no-IO test on Windows | heuristic no-op | name-based check | **fixed** |
| M5 contract type | open | unchanged (needs `contract:` PR) | open, non-blocking |

### New finding (MODERATE, non-blocking)
- **R1. A `LOCK` response with a lock comment over 8 KiB fails after the server has taken the lock.** `locks/response.rs` caps every lock field at `max_href_bytes` (8 KiB), including `activelock/owner`, the comment. Probe: 10 000-byte comment → `Err("lock field exceeds the limit of 8192 bytes")`. Subversion does not limit lock comment length. The server holds the lock, the client reports Corrupt, and a retry then fails with "already locked". get-locks and get-lock already allow `max_text_bytes` for comments. Fix: use `max_text_bytes` for `OWNER_PATH` and keep 8 KiB for the token and timeout. It can go into #48.

### Verdict: **APPROVE** (R1 to fix in #48 or a follow-up; M5 tracked as a contract PR)

Status: DONE
Summary: Every IMPORTANT and MODERATE item from the first pass is fixed and re-verified by probes on 82f7fc1. Local gates are green (118 tests). One new non-blocking issue: lock comments over 8 KiB in the LOCK response.
