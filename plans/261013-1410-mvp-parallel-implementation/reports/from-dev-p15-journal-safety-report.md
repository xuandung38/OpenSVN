# P15 journal safety: dev report

Branch `fix/p15-journal-safety`, from `origin/main` at `7b1a7f0` (includes #60, #71, #72, #77).

## Step 1: origin/main health

Run on a clean `origin/main` checkout:

| Gate | Result |
| --- | --- |
| `cargo fmt --all --check` | pass |
| `cargo clippy --workspace --all-targets -- -D warnings` | pass |
| `cargo clippy -p opensvn --features real-engine --all-targets -- -D warnings` | pass |
| `cargo test --workspace --locked --no-fail-fast` | **1 flaky failure**: `svn-ra` `transport_auth::cancel_interrupts_a_request_queued_behind_a_prompt` (failed 2 of 3 and 1 of 8 runs) |
| `cargo test -p opensvn --features opensvn/real-engine` | pass (183) |

The flaky test waited a fixed 200 ms for the first request to reach the credential prompt. On a busy
machine the TLS handshake took longer than that, so the second request could become the prompt owner.
It is fixed on this branch: the test now polls until the first prompt is really open (10 s deadline).
Afterwards it passed 10 of 10 runs. Only the test changed, not the transport.

## Step 2: changes

1. **Commit journal handling** (`engine/real/journal.rs`, `engine/real/recovery.rs`)
   - `JournalStore::load` now returns `JournalLoadError`, which has three variants:
     - `Unreadable`: an I/O error other than "not found". The journal stays where it is, the
       working copy is reported in `needs_user_action`, and the next open tries again.
     - `UnsupportedVersion { version }`: valid JSON with a numeric `version` this build does not
       know. The journal stays byte for byte untouched and nothing is finalized from it.
       `JournalStore::begin` then refuses commits with a clear `WcNeedsCleanup` error that names
       the journal format and says what to do.
     - `Corrupt`: not JSON, no numeric `version`, or the current version that does not parse. Only
       this variant is quarantined.
   - Quarantine renames the journal to the first free name among `<name>.json.corrupt`,
     `.json.corrupt.1` … `.7`, so an earlier quarantined journal is never replaced. Before this
     change, `rename_replacing` overwrote it.
   - Nothing deletes a journal on an error path any more. Before this change, a failed quarantine
     fell back to `clear_journal`. Now the journal stays, an error is logged, the working copy is
     reported, and commits stay blocked.
2. **WC lock guard** (`engine/real/locks.rs`): `HeldDir` is now a RAII guard. If the lock/unlock
   future is dropped or cancelled before `release()`, `Drop` releases the parent directory's
   `WC_LOCK` row, its ownership record and its marker. If the future is dropped while
   `wc_lock_obtain` is still running on the blocking thread, the guard is dropped there as soon as
   it finishes.
3. **Blame memory** (`engine/real/blame.rs`): per-line attribution is now
   `{ rev, author: Option<Arc<str>>, date: Arc<str> }`. It is shared by every line of a revision,
   and author names are interned across revisions. Before this change, each line held two cloned
   `String`s. The IPC `BlameLine` output is unchanged, so it still has one `String` per line at the
   boundary.

## Tests added

- recovery: a current-format journal that does not parse is set aside; a journal of an unknown
  format is kept untouched, is not finalized after two opens, and `begin` explains why; an
  unreadable journal (a directory in its place) is kept, and a later readable journal recovers
  normally; an earlier `.json.corrupt` is never replaced (the new one goes to `.json.corrupt.1`); a
  corrupt journal with no free quarantine slot stays in place and keeps blocking commits. The
  existing corrupt-journal test was renamed and adapted.
- locks: `a_dropped_working_copy_lock_or_unlock_releases_the_directory_lock`. It uses a
  `ScriptedSession` with a new `hang_locks` switch. I mutation-checked it: with the guard disabled,
  the test fails.
- blame: `lines_of_one_revision_share_author_and_date` (checks `Arc::ptr_eq` per revision and
  author interning across revisions).

## Gates on the branch (final)

| Gate | Result |
| --- | --- |
| fmt | pass |
| clippy workspace | pass |
| clippy opensvn + real-engine | pass |
| `cargo test --workspace --locked --no-fail-fast` | pass: 851 passed, 0 failed, 10 ignored |
| `cargo test -p opensvn --features opensvn/real-engine` | pass: 190 (183 before, plus 7) |

No frontend files were touched, so I did not run the frontend gates.

## Open points

- An unknown-version or unreadable journal shows up only as `needs_user_action` for the root in
  `RecoveryReport` (it has no message field). The detailed reason goes to the log and comes back in
  the error that `begin` returns.
- The blame IPC type still allocates one `String` per line for author and date at the boundary.
  Removing that would need an IPC/contract change (for example a per-revision table).
