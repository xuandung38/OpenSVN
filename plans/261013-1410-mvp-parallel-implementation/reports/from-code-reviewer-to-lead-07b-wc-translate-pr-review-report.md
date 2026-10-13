# PR #30 review — 07B svn-wc translate (M5)

Reviewer: code-reviewer. Date: 2026-10-13. Head: `d1f8742` (branch `review/pr30`, worktree `OpenSVN-worktrees/review-pr30`, read-only).
Upstream compared: `libsvn_subr/subst.c`, `libsvn_subr/time.c`, `libsvn_subr/io.c`, `libsvn_wc/translate.c` @ 1.14.5; differential runs against local `svn 1.15.0`.

## Verdict: REQUEST CHANGES

There is 1 CRITICAL issue: Windows CI is red. There are also 4 IMPORTANT parity/contract issues, each with a small fix. The core state machine is a faithful port: it is line-by-line equivalent to `translate_chunk`, `translate_keyword_subst`, `match_keyword`, `keyword_printf` and `build_keywords`, apart from the two divergences below. The golden suite passes, and the differential checks below confirm parity on URL decoding and expansion.

## Gates (run in scratch copy, macOS, stable 1.98.1)
| Gate | Result |
|---|---|
| `cargo fmt --all --check` | OK |
| `cargo clippy --workspace --all-targets --all-features --locked -D warnings` | OK on macOS |
| `cargo test --workspace --locked` | OK (svn-wc: 21 unit + 1 golden) |
| `cargo deny check` | advisories/bans/licenses/sources OK |
| **PR CI `rust (windows-2025)`** | **FAILURE**: see C1 |
| unwrap/expect/unsafe/panic in lib | none (only tests, with `allow`) |
| Mergeable onto `main` | yes (clean merge-tree) |

## CRITICAL

### C1. Windows clippy fails, so the PR claim "all clean" was only verified on macOS
Evidence: run 38373807376, job `rust (windows-2025)`:
```
error: call to `set_readonly` with argument `false`
  --> crates\svn-wc\src\translate\flags.rs:79:9
   = note: `-D clippy::permissions-set-readonly-false` implied by `-D warnings`
```
Because clippy failed, no Windows tests ran: the `not(unix)` paths in `flags.rs` and `special.rs` are unverified. The DoD requires CI on all 3 OS.
Fix: in `set_read_write`, use `#[cfg(windows)]` and add `#[allow(clippy::permissions_set_readonly_false)]` with a comment: on Windows this only clears FILE_ATTRIBUTE_READONLY. Re-run CI. Also add a `#[cfg(windows)]` test for `set_read_only`/`set_read_write` and for the `special::install` fallback.

## IMPORTANT

### I1. Fixed-length keyword fill does not stop at an interior `$` (byte divergence in both directions)
svn pads with `while (*buf_ptr != '$') *(buf_ptr++) = ' ';`, which stops at the **first** `$` after the value. `engine.rs::subst_keyword` fills up to `len-1` instead (`buf[bp+2..len-1].fill(b' ')` and `buf[bp+3+v.len()..len-1].fill(b' ')`). A buffer can contain an interior `$`: after a matched-but-failed candidate, the scanner keeps buffering past it (`next_sign`).
Differential evidence (svn 1.15.0, `svn:keywords=Rev Id`):
| input | svn | PR |
|---|---|---|
| expand `$Rev:: a$b $` | `$Rev:: 1$b $` | `$Rev:: 1   $` |
| contract `$Rev:: 5$b $` | `$Rev::  $b $` | `$Rev::     $` |
Impact: the contraction mismatch makes OpenSVN report a false "modified", and it commits bytes svn would not produce.
Fix:
```rust
// contraction
let end = buf[bp + 2..].iter().position(|b| *b == b'$').map_or(len - 1, |i| bp + 2 + i);
buf[bp + 2..end].fill(b' ');
// expansion, value fits
let start = bp + 3 + v.len();
let end = buf[start..].iter().position(|b| *b == b'$').map_or(len - 1, |i| start + i);
buf[start..end].fill(b' ');
```
Add both rows above as golden cases in `generate.py`.

### I2. Parity breaks at svn's 16 KiB chunk boundary on the normalisation path
In svn, `next_sign_off` is a **local** of `translate_chunk`, so it resets at every chunk. The read/commit path feeds `SVN__STREAM_CHUNK_SIZE` = 16384-byte chunks. The PR translates the whole buffer in one call, which the module doc states explicitly.
Differential evidence: input `"x"*16377 + "$Rev:a$Id: x $\n"` (the first `$` lands at the end of chunk 1). `svn ci` stores `...$Rev:a$Id: x $`, but PR `to_repo_normal` gives `...$Rev:a$Id$`.
The expansion path (checkout, 1.15.0) did **not** diverge for the mirror case `"x"*16377 + "$Rev:a$Id$\n"`, so svn chunks that path differently.
Fix: convert the engine into an incremental `Translator { cr_pending, kwbuf, src_format, .. }` with `push(&mut self, chunk, out: &mut impl Write)` and `finish(out)`, and reset `next_sign` per `push`. `to_repo_normal` should feed 16384-byte chunks. This change also resolves I4. Check the 1.14.5 expand path chunking before pinning `to_working`'s chunk size; see Q2.

### I3. `to_working` / `to_repo_normal` translate `svn:special` content; svn never does
`translation_required()` returns true for `special`, and then eol and keywords are applied to `link <target>`. In svn, `svn_wc__internal_translated_stream` and `svn_subst_copy_and_translate4` branch on `special` **before** any eol/keyword handling: `read_specialfile` / `create_special_file_from_stream`, raw bytes. Server-side props can combine `svn:special` with `svn:eol-style` or `svn:keywords` (for example via svnmucc), so a symlink target containing `$Id$` or a CR would be corrupted.
Fix: `if info.special { return Ok(input.to_vec()); }` in both functions (or return `Err` to force callers through `special::*`). Document it and add a unit test. Downstream note for M9/M6: svn also treats a path that is a symlink on disk as special even without the prop (`svn_io_check_special_path`).

### I4. Whole-file buffering; no streaming API
`to_working` and `to_repo_normal` take `&[u8]` and return `Vec<u8>` with capacity `n + n/16`. A file with `svn:eol-style` (for example a multi-GB CSV or log) therefore needs at least 2x its size in RAM. With no props, the API still copies (`input.to_vec()`). Phase 07 calls for `install(stream)` and status comparison by detranslation, and the 07A/M4/M9 callers will need streaming.
Fix: same `Translator` as I2. Keep the slice functions as thin wrappers, and return `Cow<[u8]>` (or let callers skip) when `!translation_required()`. "Binary untouched" already holds: svn translates whatever the props say, and no props means identity.

## MODERATE

- **M1. Fixtures leak local identity and paths (rule 7).** 30 fixtures contain `author=hxd` (a real local account) and `file:///var/folders/0n/5d8bqg7d2s381g6g71yx_5lr0000gn/T/...`. There are no real hostnames or secrets. Fix: commit with `--username alice` (verified to work for `file://`) and use a neutral repo path, or post-process the meta. Also add a path such as `sp ace/é/a b%41.txt` so `%b`/`%P` percent-decoding is golden-pinned. I verified it matches svn 1.15.0 by differential run, but no fixture covers it.
- **M2. Phantom golden branches.** For `expect_error` cases with fixed styles (`eol_lf_mixed`, `eol_crlf_mixed`, `eol_crlf_lone_cr`), `matches_real_svn_output` asserts nothing (`_ => {}`) but still increments `checked`. These cases encode the `svn propset` consistency rejection (E135000). Either expose and test a `check_eol_consistency(input) -> Result` (translate with `eol=Some(..)` and `repair=false`; svn-client propset will need it), or stop counting those cases.
- **M3. `special::install` regular-file branch: remove + non-exclusive recreate.** `remove_file(tmp)` followed by `fs::write(tmp)` (O_CREAT|O_TRUNC) follows a symlink planted at `tmp` in the window between the two calls. `svn_types::safe_join` documents "exclusive create". Fix: write into the `NamedTempFile` and `persist()` it, or reuse `svn_types::write_file_replacing`. Then only the symlink branch needs the unique name, which `symlink()` creates exclusively anyway. Doing this may also make the runtime `tempfile` dependency (moved in the 07A-owned `Cargo.toml`) unnecessary. Document that `path` must come from `safe_join`: the function itself does not check ancestors.
- **M4. Non-unix symlink fallback drops the remainder.** svn writes `first line + "\n" + rest of stream`, but `make_symlink` (not unix) writes only `link <target>`. Fix: write the original `content`.
- **M5. Errors are not wired to `SvnError` (DoD: "lỗi qua SvnError").** Add `impl From<TranslateError> for SvnError`: `InconsistentEol` maps to `Corrupt` with `svn_code=135000`, `UnknownEol` to `Corrupt` with `135001`, and `Io` goes through `SvnError::from_io`.
- **M6. `translation_required` is too broad.** `svn:keywords=foo` (no known keyword) gives true, while svn's keyword hash is NULL, so not required. The output is the same, but callers may use this predicate for fast paths. Use `Keywords::build(..).is_empty()` semantics, or document the difference.
- **M7. Minor parity.** `Civil::from_micros` uses `div_euclid`, but APR truncates toward zero for dates before 1970 (`svn:date` can be set). Also, the EPERM `reown_file` retry in `io_set_perms` is not ported. Both are low impact; document them.

## Scope / ownership (d)
- Touched: `crates/svn-wc/src/translate/**` (replaces the 1-line `translate.rs`), `crates/svn-wc/tests/translate_golden.rs`, `tests/compat/scenarios/wc/translate/**` (+`.gitattributes * -text`, correct), the dev report, `crates/svn-wc/Cargo.toml` (07A-owned: `tempfile` moved to `[dependencies]`, `proptest` added to dev; both are already workspace-pinned, and the PR body notes the move), and `Cargo.lock`.
- No changes to `svn-types` / `svn-ra-api`. OK.

## Parity checklist (a)
| Item | Status |
|---|---|
| eol repair / inconsistent-eol / unknown style / native→LF / fixed keeps CR,CRLF | OK (golden + upstream read) |
| keyword expand/contract, `$K$`, `$K: v $`, `$K:$`, 255 cap | OK |
| fixed-length `$K:: $` pad / `#` truncation | OK, except I1 |
| custom `%a %b %d %D %P %r %R %u %_ %% %H %I`, unknown `%x`, trailing `%` | OK (golden `kw_custom`) |
| abbreviations + case rules (ci prop tokens, case-sensitive text) | OK (golden `kw_case_insensitive_prop`) |
| `$Id$`/`%b`/`%P` URL decoding | OK by differential run (`a b%41.txt`, `sp ace/é`); not in fixtures (M1) |
| 16 KiB chunk artifacts | I2 |
| special bypass | I3 |
| svn:special `link ` format, readlink, rename over destination (not followed) | OK; M3/M4 |
| exec/needs-lock bit rules, symlink untouched, sync rule | OK on unix; Windows untested (C1) |

## `$Date$` timezone design: opinion
**I agree with the resolver, with three adjustments.**
1. A fixed `utc_offset_secs` is technically correct only if each caller computes it per file at `date_micros`. The doc comment says so, but nothing enforces it, and an 11U checkout loop will be tempted to compute "now" once. That gives wrong text for commits on the other side of DST (St_Johns and LA fixtures already show -0230/-0330 and -0700/-0800 flips). A resolver makes per-date evaluation the default.
2. Do not store the closure in `KeywordContext`: it derives `Clone, PartialEq, Eq, Default`. Pass it to the builder instead: `Keywords::build(prop, &ctx, tz: &dyn Fn(i64) -> i32)`, or use a small trait `UtcOffsetAt { fn offset_at(&self, unix_secs: i64) -> i32 }` with a `Utc` impl for tests and normalization. svn-wc stays free of tz dependencies. svn-client provides an impl via `jiff::tz::TimeZone::system()`, which honours `TZ` like `localtime_r`. On Windows, check that jiff's system zone mapping does not need the `tzdb-bundle-*` feature, and run `cargo deny` on the new dependency.
3. Keep the stakes in proportion: **`$Date$` text is cosmetic for compatibility.** Contraction discards the value, so status, commit content and checksums never depend on the offset. Real svn output also varies by machine **locale**: `HUMAN_TIMESTAMP_FORMAT_SUFFIX` is gettext-translated and uses locale `%a/%b`. Verified with svn 1.15.0: `LC_ALL=de_DE.UTF-8` gives `(Di., 13. Okt. 2026)` and `ja_JP.UTF-8` gives `(2026/10/13 (火))`. The C-locale English rendering is a defensible fixed choice, but document it as an intentional divergence and do not chase exactness beyond tz+DST.

## Recommended actions (order)
1. C1: Windows clippy fix + Windows tests.
2. I1: interior-`$` fill fix + 2 golden rows.
3. I3: special bypass.
4. I2 + I4 together: incremental `Translator`, 16 KiB normalisation chunks, slice wrappers.
5. M1/M2 fixture regeneration (`--username alice`, encoded path, assert or drop phantom branches).
6. M3–M5, then the timezone resolver API (it can also land in a follow-up before 11U consumes it).

## Unresolved questions
1. Which chunking does svn 1.14.5 use on the expand path (file-install before `working_file_writer`)? If it differs from 1.15.0, byte parity for 16 KiB-straddling malformed keywords cannot hold for both versions. Proposal: pin 1.15.0 and document it.
2. Lead: accept the C-locale `$Date$` suffix as an intentional, documented divergence?
3. Should `to_working`/`to_repo_normal` on `special` return the input unchanged or an error? Reviewer preference: unchanged + doc.

Status: DONE
Summary: REQUEST CHANGES. Windows CI is red (clippy `permissions_set_readonly_false`). Real svn 1.15.0 runs confirmed two byte-parity divergences (interior `$` in fixed-length keywords; 16 KiB chunk boundary on normalisation), special files get translated, and there is no streaming API. The rest of the engine matches upstream. I agree with the timezone resolver, passed as a parameter rather than stored in the context; `$Date$` is cosmetic and also locale-dependent in svn.

---

## Re-review (2026-10-13, head `16227c7`: fix `27c8c10` + merge of main incl. #38)

Verdict: **APPROVE**, on condition that `rust (windows-2025)` goes green, or someone runs `cargo clippy/test -p svn-wc` once on Windows. CI is blocked by billing, and these runs were macOS only.

| Finding | Status | Evidence |
|---|---|---|
| C1 Windows clippy | Fixed in code (`#[allow(clippy::permissions_set_readonly_false)]` + comment, `flags.rs`). **Not verified on Windows** (no CI, no local Windows target) | read diff |
| I1 interior `$` fill | Fixed (`first_dollar`). Golden `kw_fixed_interior_dollar_{expand,normalize}_lossy` | Differential re-run vs svn 1.15.0: c1 expand MATCH, d1 contract MATCH |
| I2 chunk boundary | Fixed: `engine::Translator` (`next_sign` reset per `push`), `NORMALIZE_CHUNK_SIZE=16384`, `EXPAND_CHUNK_SIZE=102400`; golden `kw_chunk_boundary_normalize_lossy` | d3 contract MATCH. New case at the expand boundary (`"x"*102393 + "$Rev:a$Id$"`): MATCH, which confirms 102400 for 1.15.0 checkout |
| I3 special bypass | Fixed: `ContentTranslator` returns identity for `special`; test `special_files_are_never_translated` | read + test |
| I4 streaming | Fixed: `ContentTranslator::push/finish`, `translate_stream(Read, Write)`; slice wrappers return `Cow` (borrowed when identity) | read |
| M1 fixtures | Fixed: `author=alice`, `file:///tmp/opensvn-golden/...`; `kw_encoded_url` covers `sp%20ace/%C3%A9/a%20b%2541.txt` | grep: no `hxd`, `/var/folders` or `/Users/` |
| M2 phantom branches | Fixed: expect-error cases now assert `check_eol_consistency` | `translate_golden.rs:66-82` |
| M3 non-exclusive temp | Fixed: regular files use `svn_types::write_file_replacing`; symlinks use a unique name + `symlink(2)` (exclusive) + rename, and the temp is removed on failure | read |
| M4 non-unix fallback | Fixed: the full content is written (`write_file_replacing(path, content)`); `#[cfg(windows)]` tests added | read |
| M5 SvnError | Fixed: `From<TranslateError> for SvnError` (135000/135001, `from_io`) | read |
| M6 translation_required | Fixed (enabled-keyword semantics) | read |
| Timezone | Done as recommended: `UtcOffsetAt<'a> = &dyn Fn(i64) -> i32` passed to `Keywords::build`, not stored in `KeywordContext`; `utc_offset_zero` for tests and normalisation | read |

Gates (scratch copy, `CARGO_BUILD_JOBS=2`): fmt OK; clippy `--workspace --all-targets --all-features -D warnings` OK; `cargo test -p svn-wc` 30 unit + 1 golden OK; `cargo deny` OK. No unwrap/expect/unsafe in lib.

Residual (non-blocking):
- `ContentTranslator::push` accepts chunks of any size. Byte parity holds only when callers feed `chunk_size()` chunks; `translate_stream` does. Consider making `push` private, or noting it in the API doc of the M6/M9 callers.
- Version caveat from the original Q1: 102400 was verified on 1.15.0 only. Pinning 1.15.0 is fine.

Status: DONE
Summary: Every finding is fixed, and the byte-parity fixes are confirmed against real svn 1.15.0, including the 102400-byte expand boundary. APPROVE once Windows clippy/test is green.
