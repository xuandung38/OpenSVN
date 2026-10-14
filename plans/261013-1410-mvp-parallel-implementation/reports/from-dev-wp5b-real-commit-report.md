# WP5b: real engine commit

Branch `feat/wp5b-real-commit`, from origin/main cbe88ab (WP1, WP2 #103, WP5a and WP6 are all merged).

## What changed
- **`engine/real/commit.rs`**: `do_commit` is wired. It runs in one actor job, in this order:
  1. Opens a session with `connect_with(WcInfo::url, ConnectOptions::at_url(ctx))` and calls
     `ensure_authenticated`. This happens before any working copy lock is taken.
  2. Opens the registered root with `local_util::open_wc_root` (`svn_client::wc::open_containing`,
     which gives the canonical root).
  3. If `auto_add_remove` is set, runs it under `with_wc_lock`. Selected request paths go through
     `local_util::target`, which refuses traversal, externals and nested working copies.
  4. Calls `svn_client::commit::commit`:
     - state dir = `RealEngine::commit_journal_dir()`
     - `CommitOptions { message, keep_locks, depth: Infinity, session_prefix, author }`
     - every selected path is committed recursively, as `FakeEngine` does (`expand_selection`)
     - `author` = `session.authenticated_user()`
- **auto_add_remove**: one local status pass over the selection (depth infinity, unversioned
  included, ignored excluded).
  - Any conflict (text, property or tree) fails with `conflict-blocks-commit` **before** anything
    is scheduled.
  - Unversioned items are added (`add_parents`, auto-props, effective ignores) except:
    - conflict marker files recorded in any conflict skel (`local_util::conflict_markers`)
    - nested working copies
    - externals
  - Missing items are scheduled for deletion with `keep_local: true` only if they pass
    `local_util::missing_is_deletable`: BASE node, `presence = normal`, and no incomplete BASE row
    on the node or any ancestor up to the root. So an incomplete folder and the content it has
    not fetched yet are never deleted (#99 item 2).
  - Parent folders added along the way are added to the commit targets. Explicitly selected items
    that stay unversioned (marker files) are dropped from the targets.
  - A selected path that is neither versioned nor on disk fails with `path-not-found`.
  - If the commit fails after this step, the scheduled adds and deletes stay in place
    (TortoiseSVN does the same).
- **Results**:
  - `Ok(None)` gives `Internal "nothing to commit"`, the same error as FakeEngine.
  - `post_commit_err` is logged with `tracing::warn`.
  - `wc_warning` is logged with `tracing::warn` and also sent as a progress sample with phase
    `"needs-cleanup"` (`current_path` = the reason). This is the needs-cleanup signal; no UI
    reads it yet. Leftover work-queue items are run by the next operation or the next open.
  - `CommitOutcomeUnknown` runs `recover_wc_commits` once, over the unattended root session
    (`RealEngine::unattended_session`, now `pub(super)`):
    - The commit's journal is the only outcome-unknown journal of this working copy, because a
      new commit is refused while one exists.
    - Journal finalized: returns `Ok(rev)`.
    - Journal aborted (the commit certainly did not happen): returns a **retryable** `Network`
      error saying nothing was committed. This deliberately differs from the brief, because a
      non-retryable "unknown" error would be untrue here.
    - Anything else (needs user, failed, no session): returns the original non-retryable
      `commit-outcome-unknown` error, with the recovery reason as its child.
- **copy_location from_wc**: `svn-client` has no working-copy-to-URL copy, so this stays
  unsupported. The error is now a user-facing `WcInvalidOperation` instead of `Internal`.
- **WC root spelling (journals)**: `open_wc_with_recovery` now opens the database through
  `wc_read::open_db_canonical` (canonicalize, then `find_wcroot`). This is the spelling
  `open_containing` uses. Before this change, a working copy registered through a symlink, a
  different case, or a Windows `\\?\` path would never have found its commit journals.
  `WcInfo::root_path` keeps the caller's spelling, so the registry is unaffected.
- **Author**: new `RaSession::authenticated_user()`, which defaults to `None`.
  - `FakeRaSession` returns its user.
  - `HttpRaSession` returns the Basic user name cached for the origin
    (`HttpTransport::authenticated_user` → `AuthManager::basic_user`).
  - Negotiate gives `None`.
  - If the typed name differs from the server's `svn:author`, recovery fails safe: it asks the user.
- **svn-client**: `WcCommitRecovery::finalized_revisions: Vec<(id, rev)>`.
- **svn-wc fix** (found by the incomplete-folder test): committing an add into an *incomplete*
  BASE parent failed in finalize ("parent ... is not committed yet") after the server already had
  the revision. An incomplete parent is now accepted, as libsvn_wc does.
- `not_wired` was removed from `engine/real/mod.rs`, since nothing uses it any more.

## Why the commit session is anchored at the WC URL, not the root
The brief asked for "a root session reparented from the WC URL session". That does not work here:
`svn_client::commit` calls `ensure_authenticated` on the session it is given, and on a root
session that sends `OPTIONS` to the repository root. With path-based authorization limited to
trunk, the root returns 403. WP2 changed `locate` for exactly this reason. So the commit is driven
the way `svn commit` drives it: through the WC-URL session, with
`CommitOptions::session_prefix` = the WC's repository path. The in-memory session (`at`) and
`HttpConnector` (`reparent`) both support this. Settling a lost MERGE answer still uses WP6's
unattended root session.

## Tests
- `engine/real/tests/commit_tests.rs`, on the real engine with `AnchoredConnector`
  (StaticConnector-like, `FakeRaSession::at` for `Anchor::Url`), 12 tests:
  - conformance `commit_and_update_cycle`, `conflict_blocks_commit_until_resolved`,
    `schedule_and_revert` (engine checkout, status, commit, update, resolve)
  - auto add and remove: a new file, a new directory, a file in a new subfolder (the parent is
    committed), a missing file
  - no auto add (refused, nothing scheduled); "nothing to commit"; a missing path
  - incomplete folder: missing content and the whole folder missing, plus an incomplete root
    with a new file
  - conflict markers: a conflict blocks the commit before anything is scheduled; markers that are
    selected explicitly are never added; commit after resolve
  - lost MERGE answer, applied → `Ok(rev)` over an unattended session; not applied → retryable
    `Network`, then the commit can be repeated
  - unsettled (no unattended session) → non-retryable error, the journal is kept with
    author `tester` and the canonical wcroot, the next commit is refused, and `open_wc` through a
    **different symlink** and through the **same symlink** finalizes it
  - `copy_location` from a WC is refused with a user-facing error
- `tests/real_recovery_failpoints.rs`, 2 new tests:
  - engine commit killed after the revision was recorded → the next engine commit finalizes
    first and commits r3
  - engine commit killed after MERGE was sent → the next commit gives `WcNeedsCleanup`, open
    finalizes it, and the next commit gives r3
- svn-ra `session_plumbing`: `the_session_reports_the_basic_user_it_authenticated_as`.
- svn-client `commit_failpoints`: asserts `finalized_revisions`.
- Non-vacuity checks, done locally and reverted:
  - Opening recovery with the old `open_db` makes both symlink tests fail.
  - Disabling the incomplete-ancestor check makes the incomplete-folder test fail.

## Gates (svn 1.15.0, OPENSVN_REQUIRE_SVN=1, merged tree)
- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets -- -D warnings`: pass
- `cargo clippy -p opensvn --features real-engine --all-targets -- -D warnings`: pass
- `cargo clippy -p opensvn --features failpoints --all-targets -- -D warnings`: pass
- `cargo clippy -p svn-client -p svn-wc --all-features --all-targets -- -D warnings`: pass
- `cargo test --workspace --locked`: 1268 passed, 0 failed, 21 ignored
- `cargo test -p opensvn --features opensvn/real-engine`: 405 passed, 0 failed
- `cargo test -p opensvn --features failpoints`: 411 passed, 0 failed (includes 6 in `real_recovery_failpoints`)
- `cargo test -p svn-client --all-features`: 153 passed, 0 failed

## Remaining
- The `needs-cleanup` progress phase is not read by the frontend yet.
- Copying a WC to a URL needs svn-client support. The error says so, but no issue has been filed.
- Auto-scheduled adds and deletes are not rolled back when the commit fails.

## Review round 1 (review 5511900998)
Merged origin/main first (already up to date).

### 1. File externals are never deleted
- `local_util::missing_is_deletable` now excludes nodes with `file_external = 1`. A missing file
  external stays missing, and the next update restores it.
- `svn-client` `delete` refuses an external, present or missing, whatever `force` and
  `keep_local` say. It covers a file external and a directory external root (an `EXTERNALS`
  row, or a BASE `file_external` node). The error is svn's E155030 with svn's exact text:
  "Cannot remove the external at '…'; please edit or delete the svn:externals property on '…'".
- The commit harvest (`collect`) refuses with the same E155030 when it meets a delete or replace
  of an external at an operation root. This catches rows written by anything else.
- Tests:
  - Oracle `local_ops_vs_svn::delete_refuses_a_file_external_like_svn`: svn 1.15 and we give the
    same stderr text, for the external present and missing, with plain, `--keep-local` and
    `--force` deletes. Rows and status match afterwards. A planted `base-deleted` row is refused
    by `collect`.
  - Engine `auto_add_remove_never_deletes_the_source_of_a_missing_file_external`: the source is
    inside the WC subtree (`trunk/README.md`). Selections `""`, `src` and the external itself
    each give "nothing to commit", youngest stays r1 and the source still exists. A commit with
    another change gives r2 and leaves the source alone. `delete_paths` gives 155030.
- Non-vacuity: with the `missing_is_deletable` change reverted, the engine test fails, because
  the delete layer's E155030 now surfaces instead of the data loss.

### 2. Recursive add skips nested working copies
- Root cause is fixed in `svn-client` `add_children`. A child directory holding its own `.svn`
  (a separate checkout or an external) is skipped at any depth with everything in it, and stays
  unversioned. The new `add_reporting` returns svn's W150002 warning for each skip; `add` keeps
  its signature.
- A target that is itself a nested WC root, or sits below one through `add_parents`, is refused
  with W150002 before anything is added.
- The engine's local add and the commit auto-add both use `add_reporting` and log the warnings.
- Known difference from svn: svn abandons the rest of the folder (in hash order) once it meets a
  nested WC. We skip only the nested WC and keep adding its siblings. The oracle test uses
  single-entry folders so the two runs are comparable.
- Tests:
  - Oracle `local_ops_vs_svn::add_skips_nested_working_copies_like_svn`: `x/nested` and
    `w/y/deeper` give the same added nodes, the same W150002 text, and equal rows and status. An
    explicit nested target is refused with 150002.
  - Engine `auto_add_never_adds_a_nested_working_copy_at_any_depth`: `wc/x/nested` and
    `wc/x/y/deeper` each hold a local file. Siblings are committed, the nested WCs are not and
    stay `?`. The same holds for a plain `add_paths`.
- Non-vacuity: with the skip disabled, both tests fail.

### Non-blocking
- Issue #111 is open for commits with changes under a switched subtree.
- svn-wc oracle `nodes_ops_vs_svn::commit_into_an_incomplete_folder_leaves_it_incomplete_like_svn`:
  rows equal svn's, and `e` stays `incomplete` at r1.

### Gates (svn 1.15.0, OPENSVN_REQUIRE_SVN=1, own target dir)
- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets -- -D warnings`: pass
- `cargo clippy -p opensvn --features real-engine --all-targets -- -D warnings`: pass
- `cargo clippy -p opensvn --features failpoints --all-targets -- -D warnings`: pass
- `cargo test --workspace --locked`: 1271 passed, 0 failed, 21 ignored
- `cargo test -p opensvn --features opensvn/real-engine`: 407 passed, 0 failed
- `cargo test -p opensvn --features failpoints`: 413 passed, 0 failed
- `cargo test -p svn-client -p svn-wc --all-features`: 384 passed, 0 failed, 1 ignored
