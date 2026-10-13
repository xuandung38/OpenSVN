# PR #28 review — feat/p02-svn-delta (svndiff codec)

Head reviewed: `7237e72` (= PR headRefOid). Reviewer: code-reviewer. Date: 2026-10-13.
Upstream reference: apache/subversion trunk `subversion/libsvn_delta/svndiff.c` (fetched, grep-verified).

## Scope
- Diff vs origin/main: only `crates/svn-delta/**` + `Cargo.lock` (31 files, ~1.3k LOC Rust + fixtures). No change in svn-types / svn-ra-api (check (c) OK).
- Gates in scratch copy: `cargo fmt --check` OK; `clippy --workspace --all-targets --all-features -D warnings` OK; `cargo test --workspace --locked` all green (svn-delta 34 tests + 1 ignored probe); `cargo deny check` OK (only "unmatched MPL-2.0 allowance" warning, pre-existing).
- Hostile probes: scratch-only `tests/hostile_review.rs` (not in PR), run `--release`.

## Hostile-input probe results (release build)
| Probe | Result |
|---|---|
| 10-byte varint overflow in header, bad magic, version 3, truncated `SV` | clean `Corrupt` errors, no panic |
| zlib/lz4 garbage, compressed > original | clean `Corrupt` |
| zlib bomb: 65 KB input -> 64 MiB new data | accepted, 268 ms, 64 MiB alloc |
| 20-byte v0 window (1 new byte + target copy 64 MiB-1) | accepted, 64 MiB output per 20 input bytes |
| 8 MiB window fed in 4 KiB chunks | **5.7 s** |
| 10 MiB fulltext (standard 100 KB windows) fed as 76-char base64 lines | **8.8 s = 1.1 MB/s** |
| 10 MiB v0 fed raw, chunk 57 / 512 / 4096 / 65536 | **17.1 s / 1.19 s / 197 ms / 0.9 ms** |
| Base64Decoder alone, 10 MiB | 1.59 s (~6.6 MB/s) |
| Zero-length op `0x80 0x00` | accepted (svn rejects) |
| Unused trailing new data | accepted (svn rejects) |
| sview_len=0 window at offset below previous view | rejected (svn accepts) |
| Empty stream (no `SVN\x00` header) + finish | accepted as empty delta (svn rejects) |
| Applier: bad window error, then more windows + end marker | finish succeeds, outcome published |

## Findings

[CRITICAL] Decoder is quadratic in chunk count — CPU DoS from untrusted server and misses perf DoD on the real path — `crates/svn-delta/src/decoder.rs:87-94` (`[carried, chunk].concat()` on every feed) + `:142` (`data[pos..].to_vec()` every feed). Every feed re-copies the whole partial window. Measured: 1.1 MB/s on 76-byte base64 lines (what XML text events produce), 0.6 MB/s on 57-byte chunks, ~50 MB/s even at 4 KiB HTTP reads. With the 64 MiB window cap a hostile server makes one window cost minutes of CPU. The `decode_throughput_v0` probe (5.6 GB/s) feeds 64 KiB chunks, which hides this. Recommendation: keep `pending: Vec<u8>` and append only (`extend_from_slice`), parse from `pending[consumed..]`, drain consumed prefix once per feed (or `BytesMut::advance`); cache the parsed header + needed `total` so an incomplete window costs O(chunk) not O(pending). Add a regression test: 1 MiB stream fed in 1-byte chunks must finish under a fixed budget, and change the probe to feed small chunks.

[IMPORTANT] Window size limits 640x looser than Subversion — memory/disk amplification — `crates/svn-delta/src/window.rs:11-13` (`MAX_INSTRUCTIONS_LEN = 4 MiB`, `MAX_VIEW_LEN = 64 MiB`), used in `decoder.rs:161-171,199-200`, `apply.rs:108-111,170`. svndiff.c:713-720 rejects `tview_len|sview_len > SVN_DELTA_WINDOW_SIZE (102400)`, `newlen > 102400+10`, `inslen > 102400*21`, and decompression limits at :574-592 use the same values. Because every upstream client rejects bigger windows, no real server sends them, so tightening is compatibility-safe. Current worst case per window ~68 MiB pending + 64 MiB decoded new data (+ a v0 `into_owned` copy) + 64 MiB sview + 64 MiB out_buf ≈ 250 MiB from one hostile window; a 65 KB zlib bomb forces a 64 MiB inflate; 20 bytes -> 64 MiB of output. Also blocks the "peak < 2 MB" DoD. Recommendation: `MAX_VIEW_LEN = 102_400`, new-data stored cap `102_400 + 10`, `MAX_INSTRUCTIONS_LEN = 102_400 * 21`, mirror svn's error text; add tests at limit and limit+1.

[IMPORTANT] Base64Decoder per-group decode is slow (~6.6 MB/s) — `crates/svn-delta/src/base64.rs:30-48`. One `decode_vec` call per 4 characters plus a push per byte. This is the update-report hot path; far below the 300 MB/s target. Recommendation: strip whitespace into a scratch buffer, decode the largest multiple-of-4 prefix in one `decode_vec` call, carry the remainder (<4 chars); padding/after-padding checks stay on the boundary group.

[MODERATE] Op validation is looser than svn: zero-length ops and unused new data accepted — `crates/svn-delta/src/window.rs:91-133`. svndiff.c:496-499 rejects `op.length == 0` ("insn %d has length zero"), :537-539 rejects `npos != new_len` ("Delta does not contain enough new data"). Zero-length ops also let 2 wire bytes become one 24-byte `TxDeltaOp` (4 MiB ins section, or ~4 KB compressed -> ~48 MiB `Vec`). `serialize_instructions` already drops zero-length ops, so the encoder stays valid. Recommendation: add both checks in `validate_ops` (encoder/`validate_window` share it, fine). Update the `parse_instructions` doc.

[MODERATE] Backward-view rule ignores `sview_len > 0` guard — `crates/svn-delta/src/decoder.rs:194-198`. svndiff.c:731-735 only checks when `sview_len > 0`; this decoder also rejects a pure-insert window (`sview_len == 0`) whose offset is below the previous view (probe confirmed). A stream upstream accepts gets rejected = interop risk, cheap to fix. Recommendation: add `h.sview_len > 0 &&` to the condition; add a test.

[MODERATE] Empty stream accepted as empty delta; upstream rejects — `crates/svn-delta/src/decoder.rs:59-70`. svndiff.c:808-811 (`error_on_early_close`, which ra_serf sets TRUE) returns "Unexpected end of svndiff input" when `header_bytes < 4`. Accepting it means a txdelta body truncated to zero bytes becomes an empty file, caught only if the caller set `expect_md5`. Recommendation: make `finish()` fail when no header was seen (or add an explicit `allow_empty` opt-in), and document that callers must pass the server's checksum.

[MODERATE] WindowApplier not poisoned after a failed window — `crates/svn-delta/src/apply.rs:101-122`. After `apply_window` returns Err (corrupt window, short base, write error), later windows still apply and `finish()` publishes `ApplyOutcome` via `ApplyHandle` with digests of incomplete content (probe confirmed). If no expected digest was set, an editor that logs and continues treats the result as valid. Recommendation: add a `failed` flag; any error sets it; later `apply_window`/`finish` return Err and never call `handle.set`.

[MODERATE] Acceptance criteria only partly met — phase-02 Success Criteria:
- Proptest roundtrip 10k + v2 golden: met (`tests/properties.rs:53`, `tests/golden.rs`).
- "golden from svn 1.14/1.15": partial. Only svn 1.15 FSFS rep bodies (3 texts x 3 compressions, generated locally by `tests/data/generate.py`), no 1.14, no wire capture from mod_dav_svn. Acceptable as interim per spec step 8; needs P06 `fixtures/svndiff/` follow-up.
- Fuzz 1h no panic/OOM: not met. Targets exist and compile; never run (PR body says so). Spec step 9 hands nightly to P13, but the DoD box stays open.
- Bench >= 300 MB/s + peak < 2 MB for 1 GB: not met. No `benches/decode.rs` (criterion missing from workspace pins, so the author deferred it). The ignored probe uses 64 KiB chunks and hides the CRITICAL finding above. Memory never measured, and it cannot pass with 64 MiB caps.
- Deviation needing a lead decision: adding criterion as a workspace dev-dep vs accepting the `#[ignore]` probe as the bench.

[LOW] `crates/svn-delta/fuzz/.gitignore` ignores `Cargo.lock`, so fuzz deps (libfuzzer-sys "0.4") are unpinned. Fine for a standalone fuzz crate; commit the lock if P13 needs reproducible nightly runs.
[LOW] `decoder.rs:208` `new_data.into_owned()` copies every v0 window again; `Bytes::copy_from_slice` is the same cost. Could avoid it by slicing a frozen `BytesMut`, ties into the CRITICAL fix.
[LOW] `apply.rs:164-184` seeks and re-reads the full source view per window even when views overlap; perf only.

## Verified correct (risk calibration)
- v1/v2 section layout matches `svn__decompress_zlib/lz4`: `varint(orig_len)` + payload, raw when `payload.len() == orig_len`, `orig_len` capped before allocation; zlib read is `take(orig+1)` so an over-long stream is detected, not buffered (`compress.rs:29-82`). lz4_flex default features (safe-decode) on.
- Varint: 10-byte cap + pre-shift overflow check, u64::MAX roundtrip tested; header 5 fields in svn order; `sview_offset + sview_len` overflow checked.
- Target-copy overlap: chunked `extend_from_within` gives the same result as svn's byte-wise copy; checked against a byte-wise reference in proptest; `offset >= tpos` rule matches svndiff.c.
- sview/tview bounds: `validate_ops` uses checked_add for all sums; the applier re-checks slices (`get(range)`), no indexing panics found in lib code; no unwrap/expect/unsafe in `src/`.
- Truncated streams: `finish()` rejects a partial header or window; golden truncation test plus my probes confirm no panic.
- Workspace patterns: `[lints] workspace = true`, doc comments on all pub items, no new workspace deps, no public contract change.

## Merge recommendation
**REQUEST CHANGES.** Blocking: the CRITICAL quadratic decoder and the IMPORTANT window caps (align with svn's 100 KB limits). The base64 throughput should land in the same pass since it is the same hot path. The MODERATE strictness fixes (zero-length ops, npos, sview_len>0 guard, empty stream, applier poison) are small, line up with upstream, and should land in this PR too. The fuzz run, the 1.14/wire golden fixtures and the criterion bench can be deferred explicitly, with the decision recorded in the plan.

## Unresolved questions
1. Lead decision: add `criterion` as a pinned workspace dev-dep for `benches/decode.rs`, or accept the ignored probe (after rewriting it to feed small chunks) as the bench?
2. Empty-stream policy: does P03 (ra_serf-like layer) ever need to accept a header-less empty txdelta? Upstream says no. Confirm before making it a hard error.
3. Fuzz 1h DoD: run once locally before merge, or defer fully to P13 nightly?

Status: DONE
Summary: PR #28 builds, lints and passes all workspace gates with no contract changes, but the streaming decoder is O(n²) in chunk count (1.1 MB/s on base64 lines, CPU DoS) and window caps are 640x looser than svndiff.c. Request changes.

---

## Re-review — head 36e0b1c (fix a232efd + merge of main), 2026-10-13

Scope since 7237e72: `crates/svn-delta/{src,tests,benches}` + root `Cargo.toml` (criterion `=0.8.2` added as a workspace dev-dep) + `Cargo.lock`. Three-dot diff vs origin/main has no svn-types/svn-ra-api change. The branch is one main merge behind (#38 rename_replacing); GitHub reports MERGEABLE. CI was ignored (Actions blocked by billing).

Local gates (scratch copy): fmt OK; `clippy --workspace --all-targets --all-features --locked -D warnings` OK; `cargo test --workspace --locked` all green; `cargo deny check` OK. Criterion benches were not run, per the coordinator's instruction because of machine load. Scratch build output cleaned (3.9 GiB).

### Earlier findings: status
| Finding | Status | Evidence |
|---|---|---|
| O(n²) decoder | **FIXED** | `decoder.rs` now append-only buffer + cached header + amortised drain. Release probe, 32 MiB v0: chunk 1 = 46 MB/s, 57 = 904, 512 = 15.5k, 4096 = 8.8k, 64K = 3.4k MB/s (was 0.6 MB/s at 57). Linear. |
| 100 KB window limits (incl. post-decompression) | **FIXED for sview/tview/new-data**, **over-tightened for instructions** (see new IMPORTANT) | tview 102400 OK / 102401 rejected. zlib declaring 64 MiB rejected before allocation. zlib or lz4 that inflates past its declared 100 bytes rejected. |
| base64 throughput | **FIXED** | Bulk `decode_vec` per call. Base64->svndiff payload: 566 MB/s at 77-char lines, 643 MB/s at 4 KiB, 71 MB/s at 7-byte chunks. Edges OK: split padding (`aGVsbG8`+`=`), interior whitespace, data after padding, mid-stream `=` all handled. |
| zero-length ops | **FIXED** | `window.rs` validate_ops -> "instruction has zero length". |
| unused new data | **FIXED** | `npos != new_len` -> Corrupt. |
| empty stream | **FIXED** | No header -> Corrupt on finish; `SVN\0` alone = empty delta (matches svndiff.c:808). |
| sview_len>0 rule | **FIXED** | pure-insert window below the previous view accepted; a real backward view is still rejected. |
| poisoned WindowApplier | **FIXED** | After a bad window: later windows Err, finish Err, `ApplyHandle::outcome()` stays None. |
| Bench DoD | Addressed | `benches/svndiff.rs` (criterion, chunk sizes + base64 lines); criterion pinned in workspace, which resolves earlier question 1. |

### New / remaining findings

[IMPORTANT] Instruction-section cap is 21x stricter than Subversion and rejects windows upstream accepts — `crates/svn-delta/src/window.rs` (`MAX_INSTRUCTIONS_LEN = MAX_WINDOW_SECTION_LEN = 102_400`; the doc comment says svndiff.c applies 102400 to the instruction section). That is not what upstream does: svndiff.c:75 `MAX_INSTRUCTION_SECTION_LEN = SVN_DELTA_WINDOW_SIZE * MAX_INSTRUCTION_LEN` = 102400 x 21 = 2,150,400, enforced at :717 and :576/:592. Probe: a valid 102400-byte window made of 1 new byte + 102399 one-byte target copies (instruction section 204,799 B) -> `Corrupt: instruction section is too large`; libsvn_delta accepts it. svn's own xdelta rarely emits instruction sections that dense, but composed or other-implementation deltas can, and the failure would be a hard update error on legitimate server data. Matching upstream costs nothing in memory, because ops are already capped by tview (every op >= 1 byte, so <= 102,400 ops ≈ 2.4 MiB). Recommendation: `MAX_INSTRUCTIONS_LEN = 102_400 * 21`, header cap `+ SECTION_SLACK`, fix the doc comment, add a test with a 204,799-byte instruction section.

[LOW] `last_view` only updates when `sview_len > 0` (`decoder.rs` build_window), but svndiff.c:773-774 updates it unconditionally. Probe: views (10,5) -> (0,0) -> (2,3) are accepted by svn and rejected here. Real producers keep offsets monotonic, so this is a quirk-level divergence. Optional: mirror upstream exactly.
[LOW] 1-byte feeds run at 46 MB/s, because each `feed` allocates its output Vec. Linear and acceptable; a `feed_into(&mut Vec)` variant like Base64's would remove the cost.
[LOW] Still open from the first pass, non-blocking: fuzz never run (P13 nightly), golden fixtures are svn 1.15 FSFS reps only (P06 to provide 1.14/wire), fuzz `Cargo.lock` gitignored.

### Final verdict
**REQUEST CHANGES — one small item.** All eight original findings are fixed and verified with probes. The only blocker is the instruction-section limit, a one-constant fix plus a test, which brings the decoder into line with svndiff.c. Once that lands with the gates still green, this is APPROVE with no further review needed beyond a diff check.

Status: DONE
Summary: 8/8 earlier findings fixed and probe-verified; gates green locally. One new IMPORTANT: instruction cap 102400 vs upstream 2,150,400 rejects svn-valid windows, a one-line fix.
