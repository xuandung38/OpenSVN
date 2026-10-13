# P03 svn-protocol M2 — PR #48 review (code-reviewer → lead)

- PR: xuandung38/OpenSVN#48 `feat: add svn-protocol update, commit, file-revs and older-server fallbacks`
- Reviewed SHA: **964a01f** (base: #40 @ 82f7fc1, which it contains)
- Scope: `crates/svn-protocol/**` (+~5.2k LOC: update, commit, file_revs, list_report, caps/fallbacks, fuzz) and `tests/compat/scenarios/protocol/update-commit.yaml`. **Owned paths only.**
- Threat model: hostile server/proxy on the session origin. References: libsvn_ra_serf 1.14.x `update.c`, `commit.c`, `merge.c`, `options.c` and mod_dav_svn `reports/update.c` (Apache 2.0).

## Verdict: REQUEST CHANGES

The update parser's path safety, memory caps and streaming are solid. But it does not enforce the editor-drive structure: a hostile server can produce event sequences that break the `DeltaEditor` ordering rules. It also drops metadata that the server treats as authoritative (response mode, copyfrom). Both are blocking because P10 will translate these events directly into `DeltaEditor` calls.

## Gates (local; CI blocked by billing; `CARGO_BUILD_JOBS=2`; scratch copy cleaned afterwards)
| Gate | Result |
|---|---|
| `cargo fmt --all --check` | ok |
| `cargo clippy -p svn-protocol --all-targets --all-features --locked -D warnings` | ok |
| `cargo test -p svn-protocol --locked` | **184 passed / 0 failed** |
| unwrap/expect/panic/unsafe in lib | none (all occurrences are inside `#[cfg(test)]`) |
| Owned paths | ok |
| Fuzz (`update_parser`, `file_revs`) | compile only, not run (nightly) |
| Windows / cargo-deny | **not verified** (CI unavailable); no new dependencies |

## Verified OK (evidence)
- **Node names go through `RelPath::from_server` on the joined path before any event** (`update/parser.rs:68-72, 103-110`). Probe: `..`, `.svn`, `.SVN`, `SVN~1`, `CON`, `a/b`, `x:y` are all rejected. Consequence: repository names containing `:` cannot be checked out. This follows `from_server`'s Windows-safety rule, but it is a product limitation the lead should be aware of.
- **`D:checked-in` hrefs** go through `accept_href`. Probe: a foreign origin and `!svn/ver/...` are rejected; real captures use `!svn/rvr`, which is accepted.
- **No whole-file buffering for text deltas**: `Base64Stream` (`util.rs`) decodes incrementally and keeps fewer than 4 pending chars. Probe: 48 MiB of base64 fed in 1 MiB pieces → 48 `TxDeltaChunk` events of at most 768 KiB each, nothing accumulated.
- **Memory caps**: href/md5 8 KiB, `set-prop` 16 MiB, node depth 256 (the XML depth limit of 128 applies first), and text outside a captured value is not buffered.
- **Chunk-split**: a proptest over the recorded fixtures (`tests/update_golden.rs:387-404`) plus the mutation smoke test.
- **`If:` header**: format `<public-url> (<token>)` matches serf `commit.c` (`maybe_set_lock_token_header`) and the captured `commit/lock-put`. Tokens containing CR/LF or `>` are rejected (probe), so header injection is not possible.
- **MERGE response**: the baseline response is preferred, `S:post-commit-err` is sanitised and capped, and the revision is required. `X-SVN-Options: release-locks no-merge-response` matches the captures.
- **Wire parity**: commit bodies are byte-exact against the captures. Update-report bodies are compared as canonical XML, which is acceptable because serf emits attributes in hash order (e.g. `update-mixed.req`). The `create-txn-with-props` skel matches the capture.
- **Capability table vs serf `options.c:178-252`**: all 14 `DAV:` tokens are recognised (depth, mergeinfo, log-revprops, atomic-revprops, partial-replay, inherited-props, reverse-file-revs, ephemeral-txnprops, inline-props, replay-rev-resource, svndiff1, list, svndiff2, put-result-checksum). `SVN-Allow-Bulk-Updates` On/Off/Prefer and `SVN-Repository-MergeInfo` are parsed.

## IMPORTANT

### U1. The update parser does not enforce the drive structure, so a hostile server can break the `DeltaEditor` ordering rules
Probes on 964a01f (all parse **OK**, with no error):
| Input | Events emitted | Rule broken |
|---|---|---|
| `<X:add-file xmlns:X="urn:x">` closing inside the root | `CloseDir("")`, then `AddFile("after")` | `end()` matches on local name only (`parser.rs:309-333`), so a foreign-namespace end tag pops the root. Children then appear after root close (rules 2 and 4) |
| two `<S:txdelta>` in one file | `TxDeltaStart/End` twice | rule 3: `apply_textdelta` at most once per file |
| `<S:txdelta>` in `add-directory` / at root before any node | `TxDeltaStart{path:"d"}` / `{path:""}` | text delta on a directory |
| `add-file` inside `add-file`; `delete-entry` inside `open-file` | `AddFile("f/g")`, `DeleteEntry("f/x")` | a file used as parent (P10 has no `DirId` for it) |
| two root `open-directory` | `OpenDir("")` twice | `open_root` called twice |

Fix: add a transition check equivalent to serf's `update.c` state table (`:100-235`):
- `end()` must verify `Ns::Svn` and that the name equals the innermost open node element.
- `add-*`, `open-*`, `delete-entry` and `absent-*` are only allowed under a `Dir`.
- `txdelta` and `fetch-file` are only allowed under a `File`, at most once, and before that file closes.
- Exactly one root.
- `target-revision` at most once, before the root.

Make the fuzz oracle assert the same invariants.

### U2. Metadata the server treats as authoritative is dropped
- **Response mode**: the root element's `send-all="true"` and `inline-props="true"` attributes are ignored. serf derives `add_props_included` and whether to GET or PROPFIND from them (`update.c:1500-1512, 1563`). mod_dav_svn forces skelta when `SVNAllowBulkUpdates Off` (`reports/update.c:1005-1023`), and in skelta it never sends `fetch-file` for added files (`:850`). Without these flags P10 cannot tell "added file with an empty delta" from "added file whose content must be fetched", which can produce silently empty files.
- **Copy source**: `copyfrom-path` and `copyfrom-rev` on `add-directory`/`add-file` (mod_dav_svn `:379-397`, serf `:1540, 1589`) are not parsed (probe: `AddFile` without a source). Yet the builder exposes `send_copyfrom_args` (`update/request.rs:59, 214`). With that option set, the server sends the text delta against the copy source, and P10 would apply it to an empty base: checksum failure or corrupt text.
- **`delete-entry rev`** (mod_dav_svn `:573`) is dropped, but `DeltaEditor::delete_entry` takes `rev`.
- Fix:
  - Emit a first event `ReportStart { send_all, inline_props }`.
  - Add `copy_from: Option<(String, Revnum)>` to `AddDir`/`AddFile`. Keep it as raw text, since the editor contract says the editor validates copy sources.
  - Add `rev: Option<Revnum>` to `DeleteEntry`.
  - Alternatively, remove `send_copyfrom_args` until copy sources are supported.

## MODERATE
- **C1. `PUT` buffers the whole svndiff in memory** (`commit/request.rs:156-165`, `svndiff: Bytes`). serf spills the body to disk (`commit.c:176`). Committing large binaries means holding the full svndiff in RAM. Accept a `Body` (`Body::Stream` already exists).
- **C2. Virtual transaction names are not distinguished.** `parse_create_txn_response` (`commit/merge.rs:15-37`) returns only the name, whether it came from `SVN-Txn-Name` or `SVN-VTxn-Name`. `UrlBuilder::txn`/`txn_root` then use `self.txn.first()`, which is the txn stub. serf uses the vtxn stubs for `SVN-VTxn-Name`. This breaks commits through write-through proxies (`SVNMasterURI`). Return `TxnName::{Txn, VTxn}` and select the stub from it.
- **C3. Directory `COPY` lacks `Depth: infinity`** (serf `commit.c:1060`). The file copy (`:935-937`, no Depth) matches. mod_dav defaults to infinity so it works, but it is a parity gap. Add a node-kind parameter.
- **C4. Lock token checks on the commit side are weaker than on the read side.** `If:` uses `bracketed_token`, which accepts `anything-goes` (probe). MERGE `lock-token` values are only checked for XML chars. Use `validate_lock_token` (requires the `opaquelocktoken:` prefix) in both places.
- **C5.** `set-prop` with an unknown `encoding` is silently treated as raw (`parser.rs:235`). #40 policy is Corrupt.
- **F1. Bulk fallback for servers without `SVN-Allow-Bulk-Updates` and without `inline-props`.** serf uses send-all (`update.c:2651-2671`); the PR uses skelta without inline props, i.e. a PROPFIND per added node plus a GET per file. Only pre-1.8 servers are affected (1.8+ always send the header), so the impact is low within the 1.8–1.15 target. Document it or align with serf. Also, serf always sends `S:include-props yes` in skelta mode (`:2690`); harmless.
- **F2. `svndiff_version()` always prefers 2 when announced** (`caps/fallbacks.rs:34-43`). serf `negotiate_put_encoding` (`commit.c:1869-1910`) prefers svndiff1 unless compression is auto and the connection is low-latency, because svndiff2 (LZ4) compresses worse over a WAN. Expose both flags and let P10 decide by latency, or default to 1.
- **R1 (from #40, still present):** a `LOCK` response comment is capped at 8 KiB (`locks/response.rs`), so the lock succeeds on the server but the client reports Corrupt.
- **Fuzz `update_parser`:**
  - It only checks path safety on 6 event kinds; AbsentDir/AbsentFile/RemoveProp/FetchFile/TxDelta*/Close* are not covered.
  - It uses a single split point and no txn stubs.
  - It asserts no structural invariants (see U1).
  - It has not been run. Extend it, then run it for at least 1h when nightly is available.

## Low
- `skel.rs:10-15`: the bare-atom rule is stricter than svn's `use_implicit` (which also limits length to under 100), so some unusual values go explicit-length. Valid either way, so this is only a byte-level parity difference.
- `parse_update`/`UpdateParser::new` take an owned `UrlBuilder` clone per parser; fine.

## Plan / DoD follow-ups (lead decides)
- Done for M2: update-report builder and parser (send-all and skelta), commit builders and MERGE parse, file-revs, list-report, capability fallbacks, snapshots, wire parity (update and commit), proptest chunk-split, fuzz targets (compile only).
- Open: U1/U2 before P10 integrates; 1.15-server and VisualSVN goldens; fuzz 1h; compat Docker job (P06); Windows CI once billing is restored.

## Recommended actions
1. U1: transition validation in `UpdateParser`, plus negative tests from the probe table and fuzz invariants.
2. U2: `ReportStart{send_all, inline_props}`, `copy_from` on adds, `rev` on delete (or drop `send_copyfrom_args`).
3. C1/C2: streaming PUT body; Txn/VTxn kind.
4. C3–C5, F1–F2, R1.

## Unresolved questions
1. Should the parser (here) or the driver (P10) own the drive-ordering validation? I recommend the parser, because it has the element context; P10 would only see a flat stream.
2. Repository names with `:` (legal on Linux servers) fail checkout because of `from_server`. Accept as an MVP limitation, or add a per-OS rule in svn-types (contract)?
3. Can #48 merge before Windows CI and cargo-deny run (blocked by billing)?

Status: DONE
Summary: REQUEST CHANGES on #48 @ 964a01f. Local gates are green (184 tests) and path validation, caps and streaming hold, but probes show the update parser lets a hostile server break DeltaEditor ordering (U1) and drops send-all/inline-props/copyfrom data (U2). Plus 8 moderate items (non-streaming PUT, VTxn, svndiff2 preference, among others).

---

## Re-review @ 06de1ca (2026-10-13)

Commit under review: `06de1ca fix(svn-protocol): enforce update drive order, pass copy sources and delete revisions, stream PUT bodies, virtual transactions`. CI is blocked, so gates ran locally on a `git archive` copy with `CARGO_BUILD_JOBS=2`: fmt ok; `clippy -p svn-protocol --all-targets --all-features --locked -D warnings` ok; `cargo test -p svn-protocol --locked` **206 passed / 0 failed**; no unwrap/expect outside `#[cfg(test)]`. The scratch copy was cleaned afterwards (`cargo clean`). The compat gate is separate (after #42) and was not run.

### First-pass probes, re-run
| Probe | Before | Now |
|---|---|---|
| foreign-ns end tag inside root | root closed early, child after root close | foreign element ignored; `after` is a child of root, then `CloseDir("")` last ✔ (role stack, `parser.rs:46-53, 462-509`) |
| two txdelta per file | accepted | `Corrupt: a file has more than one text delta` ✔ |
| txdelta in dir / at root | accepted | `Corrupt: file content outside a file` ✔ |
| child under file / delete in file | accepted | `Corrupt` ✔ |
| two roots | accepted | `Corrupt: …continues after the root directory closed` ✔ |
| copyfrom on add-file | dropped | `AddFile{copy_from: Some(("/trunk/a.txt", 3))}` ✔ (raw text; per the editor contract the editor validates it) |
| delete-entry rev | dropped | `DeleteEntry{rev: Some(4)}` ✔ |
| response mode | ignored | first event `ReportStart{send_all, inline_props}` ✔; skelta adds get synthesised `FetchFile` (files) and `FetchProps` (adds without inline props), matching serf `update.c:1563` ✔ |
| set-prop unknown encoding | treated as raw | `Corrupt` ✔ |
| invalid name | UnsafePath | UnsafePath, with the full path in the message ✔ |

### New hostile variants (beyond the two requested)
| Variant | Result |
|---|---|
| V1 txdelta then fetch-file in one skelta file | `Corrupt` ✔ |
| V2 txdelta hidden inside a foreign wrapper element, then a second txdelta | `Corrupt: file content outside a file` ✔ (role must be Node) |
| V3 `<S:add-file>` smuggled inside a txdelta capture | markup ignored, no node event ✔ |
| V4 node after root close (outside the root) | `Corrupt` ✔ |
| V5 target-revision after root start | `Corrupt` ✔ |
| V6 copyfrom on open-file | `Corrupt: inconsistent copyfrom attributes` ✔ |
| V7 set-prop under a foreign wrapper | `Corrupt` ✔ |
| V10 root open-directory nested under a foreign element | `Corrupt: root is misplaced` ✔ |
| sticky failure (error in feed → feed/finish) | stays failed ✔ |
| **V9 skelta + inline-props, open-file with a prop change only** | emits a synthesised **`FetchFile`** for an *opened* file ✘ (see N1) |
| V8 two `D:href` in one `D:checked-in` | the last one silently wins (see N2) |

### Other fixes verified (code + tests)
- **Streaming PUT**: `put_request(.., svndiff: Body, ..)` plus `ReadBody<R: Read>` (64 KiB pieces) ✔
- **VTxn**: `parse_create_txn_response` → `TxnName{name, virtual_}`; `txn`/`txn_root` select the vtxn stubs via `stub_for` ✔. Note that `From<&str>` yields a *plain* name, so callers must pass the parsed `TxnName` through. It is documented; keep it in mind for P10.
- **COPY**: `Depth: infinity` for directories only (`src_kind == Dir`), matching serf `commit.c:1060` ✔
- **Lock tokens**: `If:`, `UNLOCK` and MERGE `lock-token` all use `validate_lock_token` (requires the `opaquelocktoken:` prefix) ✔
- **svndiff preference**: `svndiff_version(low_latency)` = 1 unless low latency (or only 2 is available), matching serf `negotiate_put_encoding` with compression auto ✔
- **Bulk fallback**: Auto + no `SVN-Allow-Bulk-Updates` + no inline-props → send-all, matching serf `update.c:2651-2671` ✔
- **R1**: the LOCK owner/comment is truncated at 16 MiB on a char boundary, never an error; get-locks owner is also allowed 16 MiB ✔
- **Fuzz `update_parser`**: now asserts `DriveChecker` invariants on every event (compile only; not run, needs nightly).

### Remaining (non-blocking)
- **N1 (MODERATE)**: in skelta mode a `FetchFile` is synthesised for **opened** files with no `fetch-file` (`parser.rs:480-486`). mod_dav_svn sends `fetch-file` only for opened files whose text changed (`reports/update.c:850`), and serf fetches opened files only on `fetch-file`. The result is a needless full GET for every prop-only change (with `base_checksum: None`), and for every opened file in a skelta `status -u`. Not corruption, since the content equals the base, but wasted traffic. Fix: synthesise only when `node.intro == Intro::Add`, and add V9 as a test.
- **N2 (LOW)**: a repeated `D:href` inside `D:checked-in` overrides the first. Reject a second href (hostile-only; a single href can already point anywhere inside the repository).
- Unchanged from the first pass: names containing `:` cannot be checked out (product decision for the lead); Windows CI and cargo-deny are pending billing; 1h fuzz needs nightly.

### Verdict: **APPROVE**
U1 and U2 are fixed and every hostile variant is rejected or harmless. N1 is a traffic optimisation that can land in a follow-up or in the first P10 integration PR.

Status: DONE
Summary: Every #48 finding is verified fixed at 06de1ca. Local gates are green (206 tests), the first-pass U1/U2 probes and 10 new hostile variants pass, and the scratch copy is cleaned. Open non-blocking items: N1 (a needless FetchFile for opened files in skelta mode) and N2 (a repeated checked-in href overrides the first).
