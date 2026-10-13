# PR #32 review — 07B svn-wc pristine store (M4)

Reviewer: code-reviewer. Date: 2026-10-13. Head: `0c06718` (`feat/07b-wc-pristine`, worktree `OpenSVN-worktrees/review-pr32`, read-only).
Upstream compared: `libsvn_wc/wc_db_pristine.c`, `wc-queries.sql`, `wc-metadata.sql` @ 1.14.5.

## Verdict: REQUEST CHANGES

There are 2 CRITICAL issues: Windows CI is red, and the GC/install contract can delete a referenced pristine. There are also 3 IMPORTANT issues. The layout, checksum string forms, streaming hashing and mismatch cleanup are correct.

## Gates
| Gate | Result |
|---|---|
| fmt / clippy `--all-features -D warnings` (macOS) / `cargo test -p svn-wc` (8 pass) / `cargo deny` | OK |
| **PR CI `rust (windows-2025)`** | **FAILURE**: `clippy::permissions_set_readonly_false` at `pristine/mod.rs:336` (same lint as PR #30) |
| unwrap/expect/unsafe in lib | none (`unwrap_or` only) |
| Owned paths | `src/pristine/**` (replaces the 1-line `pristine.rs`), `Cargo.toml` (07A-owned; `tempfile` moved to `[dependencies]`, the same line as in #30, which the PR body notes). No contract crate changes. |

## Checklist vs request
| # | Item | Status |
|---|---|---|
| 1 | `pristine/<xx>/<sha1>.svn-base`, temp in `<adm>/tmp`, `$sha1$`/`$md5 $` | OK. Matches `PRISTINE_STORAGE_RELPATH`/`_EXT`/`PRISTINE_TEMPDIR_RELPATH` and `svn_checksum_serialize` |
| 2 | temp → fsync → rename → read-only; Windows rename retry | Partial: the order is right, but there is no Windows retry and no dir fsync (I1, M1) |
| 3 | Hash while streaming; mismatch leaves nothing | OK. The `NamedTempFile` drop removes the temp on every error path; there is a test for this |
| 4 | Format 32 dehydrated read-only | Reading is OK (`open` gives `NotHydrated`). Nothing prevents writes on a format-32 store (I3) |
| 5 | Index contract covers ACTUAL_NODE; refcount>0 never deleted | The ACTUAL_NODE clause is documented, but **refcount>0 deletion is possible** (C2) |
| 6 | Gates / owned paths | Windows red (C1) |

## CRITICAL

### C1. Windows clippy failure
`remove_unreferenced` calls `perms.set_readonly(false)` under `#[cfg(windows)]`, and `-D warnings` turns the lint into an error. Fix: `#[allow(clippy::permissions_set_readonly_false)]` with a comment explaining it only clears FILE_ATTRIBUTE_READONLY. Do not swallow the error with `let _`: map it, or log it via `tracing::warn!`.

### C2. GC deletes the file before an atomic conditional row delete, so a referenced pristine can be lost
Upstream (`pristine_remove_if_unreferenced_txn`) runs `DELETE FROM pristine WHERE checksum=?1 AND refcount=0` **first**, inside an sqlite transaction under the WC write lock, and removes the file **only if a row was deleted**. Install also decides based on the **row** (`STMT_SELECT_PRISTINE`) inside a transaction.
The PR does the reverse:
- `remove_unreferenced` takes a snapshot `unreferenced()`, then `remove_file`, then an unconditional `forget()`.
- `install` checks `dest.exists()` on the filesystem, writes nothing if the file exists, then calls `record()` ("existing row left untouched").

Race (two operations, or a crash and resume): GC snapshots X → install sees X's file exists, skips the write, and a NODES row now references X (refcount 1) → GC removes the file and forgets the row. Result: a node points at a missing pristine, which is WC corruption (`svn cleanup` cannot fix it; a re-checkout is needed). Even without concurrency, `forget()` being unconditional means the trait cannot guarantee "refcount>0 never deleted".
Fix (trait change; do it now, before `WcDb` implements it):
```rust
/// Atomically deletes the row iff refcount = 0 (and no ACTUAL_NODE reference).
/// Returns true when a row was deleted; only then may the caller remove the file.
fn forget_if_unreferenced(&mut self, sha1: &Sha1Digest) -> Result<bool, PristineError>;
```
GC loop: `if index.forget_if_unreferenced(sha)? { remove_file(path) (ENOENT ok) }`. A crash between the two steps leaves an orphan file, which is harmless because install overwrites orphans (see I2). Document that `install` and `remove_unreferenced` require the WC write lock, and that `record` plus the NODES update must share one transaction (the 07A facade owns this). Add a test with an index whose refcount changes between `unreferenced()` and `forget_if_unreferenced()`.

## IMPORTANT

### I1. No Windows rename retry or read-only handling
`tmp.persist(&dest)` uses MoveFileExW(REPLACE_EXISTING) once. svn's `svn_io_file_rename2` on Win32 retries on sharing violation / access denied (`WIN32_RETRY_LOOP`), and it makes a read-only destination writable first. Every stored pristine is read-only, and antivirus or indexers routinely hold new files briefly, so a plain rename fails on real Windows machines. This also becomes mandatory once I2 overwrites orphan files. Fix: a `rename_replacing(src, dst)` helper: on Windows, clear read-only on `dst` if it exists, then retry ERROR_ACCESS_DENIED/ERROR_SHARING_VIOLATION with backoff (svn uses about 10 s total). Unix: a single `rename`. Ideally put it in a shared location; `svn_types::write_file_replacing` already exists, so propose a small contract PR rather than a third copy.

### I2. Install decides by file existence, not row existence, and never overwrites orphans
svn: if the row exists, keep the stored file and delete the new temp. Otherwise rename over any orphan file ("it is an orphan file and it doesn't matter if we overwrite it"). The PR keeps an existing orphan **unverified** and then records a row for it. A truncated orphan (from an svn crash: svn renames without fsync), or one damaged externally, gets blessed as valid. Fix: `match index.lookup(sha)? { Some(row) if row.hydrated => drop temp, None or dehydrated => fsync + rename_replacing + set read-only + record }`.

### I3. Format 32: no write guard, and `record` cannot re-hydrate
`install` always records `hydrated: true`, but `record` "leaves an existing row untouched". On a dehydrated row, the file gets written and the row stays `hydrated=0`, so `open` keeps returning `NotHydrated`. The plan says format 32 is **read-only**: `PristineStore` should know the WC format and reject `install`/`remove_unreferenced` (`WcFormatUnsupported`) for format 32. Or, if hydration is ever allowed, `record` must upsert `hydrated=1`.

## MODERATE
- **M1. No parent-directory fsync after the rename** (unix). The module doc promises "never a row without a file", but sqlite's fsynced commit can survive a power loss while the un-fsynced rename does not. svn has the same gap. Either fsync `pristine/<xx>/` after `persist` (cheap) or soften the doc.
- **M2. Errors are not routed through `SvnError` (DoD).** Add `From<PristineError> for SvnError`: `ChecksumMismatch` maps to `Corrupt` with `svn_code` 200014 (SVN_ERR_CHECKSUM_MISMATCH), `NotHydrated` to a code that tells P11 to fetch via RA, and `Io` goes through `SvnError::from_io`. `Index(String)` loses the cause; consider `Box<dyn Error + Send + Sync>` or `SvnError`.
- **M3. The ACTUAL_NODE clause is stricter than svn.** 1.14.5 GC uses only `refcount = 0` (`STMT_SELECT_UNREFERENCED_PRISTINES`); ACTUAL_NODE `older/left/right_checksum` are legacy columns without triggers. Keeping that clause is safe because it only retains more, but document that it is intentionally conservative so the `WcDb` implementation does not invent extra semantics.
- **M4. `verify` re-hashes only SHA-1.** That is fine for the key. Optionally also check `size` against the row; this is cheap and catches truncation before hashing.
- **M5. The `MemIndex` test fake does not model refcount changes or concurrency.** Add the C2 race test and a test for the dehydrated-row install from I3.

## Recommended actions
1. C1 lint fix (shared with PR #30).
2. C2 trait change (`forget_if_unreferenced`, row first, then file) plus lock/transaction documentation. Coordinate with 07A before #29's `WcDb` implementation.
3. I2 row-based install decision with orphan overwrite; I1 Windows `rename_replacing` with retry.
4. I3 format guard; M1–M5.

## Unresolved questions
1. Where should `rename_replacing` live? `svn-types` already has `write_file_replacing`, which is a contract crate, so the lead must decide. 
2. Should `PristineStore` take the WC format, or a `writable: bool`, from the 07A facade?

Status: DONE
Summary: REQUEST CHANGES. Windows CI is red (same clippy lint as #30). GC removes the file before an unconditional row delete, and install trusts the file's existence instead of the row, so a referenced pristine can be lost. The fix is a `forget_if_unreferenced` trait method plus a row-based install that overwrites orphans. The Windows rename retry and the format-32 write guard are also missing. The layout, checksum formats, streaming verification and mismatch cleanup are correct.

---

## Re-review (2026-10-13, head `fe81f14`: fixes `89840a4`, `c821bb1`, `fe81f14` + #38 `rename_replacing`)

Verdict: **APPROVE**, on condition that `rust (windows-2025)` goes green, or someone runs it once on Windows. CI is blocked by billing, and these runs were macOS only.

| Finding | Status | Evidence |
|---|---|---|
| C1 Windows clippy | Fixed in code (`clear_read_only_for_delete`, `#[allow]` + comment; error propagated, not swallowed). **Not verified on Windows** | read |
| C2 GC ordering | Fixed: the trait now has `forget_if_unreferenced -> bool` (atomic `DELETE ... refcount = 0` + ACTUAL_NODE check); GC removes the row first and removes the file only on `true`; write-lock requirement documented | tests `gc_keeps_a_text_that_gained_a_reference_after_the_snapshot`, `gc_removes_only_unreferenced_texts` |
| I1 Windows rename | Fixed via the `svn_types::rename_replacing` contract (#38): clears read-only, retries PermissionDenied/32/33 with backoff, about 2.5 s total | read `safe_join.rs:123-165` |
| I2 row-authoritative install | Fixed: a hydrated row keeps the stored file; no row means rename over the orphan | tests `install_overwrites_an_orphan_file_that_has_no_row`, `install_trusts_the_row_and_keeps_the_stored_file` |
| I3 format 32 | Fixed: `PristineMode::ReadOnly` gives `ReadOnlyFormat`; a dehydrated row gives `RehydrateUnsupported` | tests `read_only_mode_reads_but_refuses_writes`, `dehydrated_row_cannot_be_reinstalled_in_this_version` |
| M1 dir fsync | Fixed (`sync_dir` on unix) before `record` | read |
| M2 SvnError | Fixed (+ private code so `NotHydrated` stays distinguishable) | test `errors_convert_to_svn_errors` |
| M3 ACTUAL_NODE doc | Fixed (documented as intentionally stricter) | trait doc |
| M4 verify size | Fixed | `verify_detects_truncation_by_size` |
| M5 tests | Fixed, plus failpoint `pristine.install.before_rename`: nothing is left (no row, no file, empty tmp) | `tests/pristine_failpoints.rs`, run with `--features failpoints`: OK |

Gates: fmt OK; clippy all-features `-D warnings` OK; `cargo test -p svn-wc` 15 OK (+1 with `--features failpoints`); `cargo deny` OK. Owned paths: `src/pristine/**`, `tests/pristine_failpoints.rs`, `Cargo.toml` (the same `tempfile` line as #30).

Residual (non-blocking):
- `rename_replacing` errors are rewrapped as `io::Error::other(e.to_string())`, which loses the io `ErrorKind` (DiskFull/PermissionDenied classification). Map the `SvnError` through directly (`PristineError::Svn`) or keep the kind.
- The Windows retry budget is about 2.5 s; svn's `WIN32_RETRY_LOOP` allows about 10 s. That is acceptable, but it is a tuning knob if antivirus-heavy users report failures.

Status: DONE
Summary: Every finding is fixed. The GC/install race is closed by the conditional row delete and the row-authoritative install, with tests. APPROVE once Windows CI is green.
