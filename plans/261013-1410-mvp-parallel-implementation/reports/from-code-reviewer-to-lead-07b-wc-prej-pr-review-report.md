# PR #65 review — 07B `.prej` text generator (`conflicts/prej.rs`)

Reviewer: code-reviewer. Date: 2026-10-13. Head `308fe73` (own diff vs `cad2643`: `conflicts/prej.rs`, `conflicts/prej/tests.rs`, `conflicts/mod.rs` +1, `crates/svn-wc/Cargo.toml` +2, `Cargo.lock`, 8 fixture triples + `generate.py`).
Upstream compared at 1.15.0 (identical to 1.14.5 for these functions, diff-checked): `libsvn_wc/props.c` (`generate_conflict_message`, `prop_conflict_new`, `svn_wc__create_prejfile`), `libsvn_subr/utf.c` + `utf8proc.c` (`svn_utf__fuzzy_escape`), `libsvn_subr/io.c` (`svn_io_is_binary_data`), `libsvn_diff/diff_memory.c` (marker EOL).

## Verdict: REQUEST CHANGES (small; MODERATE only)

Correctness of the working copy is not affected: `.prej` is diagnostic text that svn never reads back. However, two extra svn 1.15.0 fixtures I recorded show **byte divergences that contradict the PR's parity claim**, and the module doc describes control-character handling that svn does not do. Both fixes are about 30 lines; please also commit my two fixtures.

## Gates (`CARGO_BUILD_JOBS=2 OPENSVN_REQUIRE_SVN=1`, macOS)
| Gate | Result |
|---|---|
| fmt | OK |
| clippy `--workspace --all-targets --all-features -D warnings` | OK |
| `cargo test -p svn-wc --locked` | OK (119 unit; svn-backed suites ran) |
| `cargo deny check` | OK |
| unwrap/expect/unsafe in `prej.rs` | none |
| Fixture hygiene | `file:///tmp/opensvn-golden-prej/repo` only; no user names or host paths |

## 1. Message branches vs `generate_conflict_message`: OK
All 11 sentences and their selection logic match byte for byte:
- **add** (2): already exists / locally deleted.
- **delete** (4): locally added / locally modified / locally deleted with a different value / local value different.
- **change** (5): conflicts with the incoming change / already changed to a different value / locally deleted / locally added with a different value / does not exist locally.

Where svn has `SVN_ERR_ASSERT_NO_RETURN` (abort), the PR falls through to the generic wording. That is the right choice for a library. Unit test `every_message_branch` covers all 11.

## 2. diff3 usage, binary and plain fallbacks
- `is_binary` equals `svn_io_is_binary_data`: lone-BOM exception, NUL means binary, `>850‰` odd bytes. Empty means text, as in svn.
- Binary on any side → plain listing; plain listing prints **raw** values; "Cannot display: property value is binary data"; empty side skipped. All OK.
- Markers, separator, `ModifiedOriginalLatest` style, and diff order (base = incoming "changed from", modified = mine, latest = incoming) are OK. The marker EOL falls back to the platform EOL in both (svn `diff_memory.c` L1092–1097 uses `APR_EOL_STR`; svn-diff `fallback_eol = native_eol()`), so Windows output also matches.
- **M1 (divergence, verified): svn computes the diff3 on the RAW values and only renders the fuzzy strings. The PR diffs the fuzzy strings.** svn: `svn_diff_mem_string_diff3(&diff, incoming_base, mine, incoming)` on raw data, then `svn_diff_mem_string_output_merge3(..., *_ascii, ...)`. When two values differ only in characters that fuzz to the same ASCII, svn still shows markers but the PR finds no conflict and prints the plain listing. Fixture recorded with svn 1.15.0 (update, base `x`, mine `café`, incoming `cafe`):
  ```
  svn : ...value.\n<<<<<<< (local property value)\ncafe\n||||||| (incoming 'changed from' value)\nx\n=======\ncafe\n>>>>>>> (incoming 'changed to' value)\n
  ours: ...value.\nLocal property value:\ncafé\n\nIncoming property value:\ncafe\n\n
  ```
  Fix: decide `has_conflicts` with `merge3` on the raw bytes, then render the output with the fuzzy strings using the raw run's hunk ranges, as svn applies the raw diff to the ASCII strings. Fuzzing keeps the line count as long as M2 is fixed (no newline rewriting), so the ranges stay valid. A simpler acceptable variant: run `merge3` on raw values for the decision, and if it conflicts, run it again on the fuzzy values for the text. That variant only diverges in pathological cases where fuzzing merges lines.

## 3. NFKD fuzzy rendering (`fuzzy_escape`)
svn uses `utf8proc_decompose(..., DECOMPOSE | STABLE | COMPAT | LUMP | NLF2LF | STRIPCC | STRIPMARK)`. Because this is `decompose`, not `utf8proc_map`, the NLF/STRIPCC rewriting in `utf8proc_normalize_utf32` never runs. In the result loop, `cp > 0 && cp < 127` passes through, `0` becomes `\0`, invalid bytes become `?\XX`, combining marks are dropped, invalid code points become `{U?XXXX}`, and the rest becomes `{U+XXXX}`.
- **M2 (divergence, verified): control characters.** The PR maps CR/CRLF to LF, tab/FF to a space, and drops other controls (including NEL and DEL); its doc states this as svn behaviour. svn keeps every C0 control except NUL as-is and renders NEL (U+0085) / DEL (U+007F) as `{U+0085}` / `{U+007F}`. Fixture (mine `m\tb\fc\vd<NEL>e`): svn `m\tb\fc\vd{U+0085}e`, PR `m b cde`.
- **M3 (approximation, documented): LUMP.** I verified in the same fixture that svn turns `’` into `'`, `–` into `-` and NBSP into a space. Double quotes `“ ”` are **not** lumped; they stay `{U+201C}{U+201D}`. The PR gives `{U+2019}` and `{U+2013}`; NBSP already becomes a space through NFKD. utf8proc's LUMP set is small: Zs→space, `2018 2019 02BC 02C8`→`'`, Pd + `2212`→`-`, `2044 2215`→`/`, `2236`→`:`, `2039 2329 3008`→`<`, `203A 232A 3009`→`>`, `2216`→`\`, `02C4 02C6 2038 2303`→`^`, Pc + `02CD`→`_`, `02CB`→`` ` ``, `2223`→`|`, `223C`→`~`. Pd/Pc/Zs are about 50 code points, which fits a static table with no new dependency. Implementing it is cheap and removes a documented divergence; keeping it as documented is acceptable if the lead prefers.
- Minor: a surrogate encoded in UTF-8 (`ED A0 80`) is `{U?D800}` in svn and `?\ED?\A0?\80` in the PR. This is pathological and fine to document.

## 4. Documented approximations: acceptable?
| Approximation | Opinion | Evidence |
|---|---|---|
| Bytewise prop order vs APR hash order | **Acceptable**: svn's own order is not deterministic | Same 5-prop conflict regenerated 4 times with svn 1.15.0: `p1..p5`, `p1..p5`, **`p4 p5 p1 p2 p3`**, `p1..p5` (APR ≥1.4.6 seeds its hash per table). Sorting gives a stable, reviewable superset of svn's behaviour |
| LUMP quotes/dashes | Acceptable but cheap to fix (M3) | see above |
| C-locale wording | Acceptable; consistent with the `$Date$` and marker-label policy | — |
| Legacy 1.7/1.8 `prej-install` skel unsupported | **Acceptable** | Only queues written by 1.7/1.8 carry it. The runner already hands it to `write_prop_reject_file(.., legacy)`; the `WcDb` implementation should return a clear "run `svn cleanup` with the client that created the working copy" error rather than silently writing nothing. Document that in the `WorkingCopyOps` doc |
| **Undocumented:** fuzzy diff (M1) and control handling (M2) | **Not acceptable as is**: these contradict the stated parity, and they are easy to fix | 2 fixtures |

## 5. New dependencies
- `svn-diff` (internal): its `[dependencies]` contain `svn-types` only, and neither crate depends on svn-wc, so **there is no cycle**. The graph svn-wc → svn-diff → svn-types fits the plan's crate layering. `crates/svn-wc/Cargo.toml` is 07A-owned; the PR body notes the change.
- `unicode-normalization =0.1.25`: already workspace-pinned and in `Cargo.lock` (via `url`/`idna`). It is MIT OR Apache-2.0, pure Rust, no C, and `cargo deny check` (licenses/bans/sources) passes. No new transitive crates.

## Extra fixtures (recorded by reviewer with svn 1.15.0; please add to `generate.py`)
1. `update_fuzzy_equal_sides`: base `x\n`, mine `café\n`, incoming `cafe\n`. svn output is in M1.
2. `update_controls_and_lump`: base `x\n`, mine `m\tb\x0cc\x0bd\xc2\x85e\n`, incoming `t’s – x<NBSP>y “q”\n`. svn output:
   `<<<<<<< (local property value)\nm\tb\fc\vd{U+0085}e\n||||||| (incoming 'changed from' value)\nx\n=======\nt's - x y {U+201C}q{U+201D}\n>>>>>>> (incoming 'changed to' value)\n`

Recipe: `svn propset p -F <file>` in two working copies, commit one, `svn up --accept postpone` the other, then read `<file>.prej`.

## Recommended actions
1. M1: conflict decision on the raw values; render fuzzy.
2. M2: fix `fuzzy_escape` control handling to match `utf8proc_decompose` (keep controls, `\0` for NUL, `{U+0085}`/`{U+007F}`), and correct the module doc.
3. Add the 2 fixtures above. Optionally implement M3 (LUMP table).
4. Document the legacy-skel error path in `WorkingCopyOps::write_prop_reject_file`.

## Unresolved questions
1. Lead: implement LUMP now (about 60 static entries) or keep it documented? Reviewer preference: implement, since it is cheap and removes the last known text divergence apart from ordering.

Status: DONE
Summary: REQUEST CHANGES (MODERATE only, diagnostic text). The 11 branches, binary detection, plain fallback, markers and EOL match svn, and the deps are cycle-free with OK licenses. Two new svn 1.15.0 fixtures show byte divergences: the diff3 decision is made on fuzzy instead of raw values, and control characters are rewritten where svn keeps them. Prop ordering is acceptable because svn's own order is nondeterministic (observed); the legacy skel is acceptable.
