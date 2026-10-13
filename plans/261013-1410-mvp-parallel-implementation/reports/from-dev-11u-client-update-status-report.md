# 11U svn-client update/checkout/status/diff: PR #74 completion report

Branch `feat/11u-client-m1`, origin/main merged in (merge commit, no rebase).

## What changed

### svn-wc owns all SQL (new facade API)

- `crates/svn-wc/src/db/bindings.rs`: `WcDb` implementations of the track B traits
  - `DbPristineIndex` implements `PristineIndex` (format 32 `hydrated` aware; ACTUAL_NODE checksum guard on forget).
  - `DbWorkQueue` implements `WorkQueueStore` on top of `wq_record_and_fetch_next` (PR #66), so the delete, the fileinfo and the fetch happen in one transaction.
  - `DbWorkingCopyOps` implements `WorkingCopyOps`:
    - `install_info` resolves the location through `read_repos_info`, so added and copied nodes are covered.
    - `.prej` files come from `conflicts::prej::prej_text` and are written with temp+rename. A legacy 1.7/1.8 skel is refused with `WcNeedsCleanup`.
    - `base_remove` with a not-present revision runs in one transaction and can be repeated.
    - `file-commit` goes to a handler set with `with_file_commit`. Without one, it fails with `WcNeedsCleanup` and the item stays queued.
  - `WcDb::pristine_store()`, `WcDb::run_work_queue(cancelled)` and `WcDb::run_work_queue_with(&mut ops, cancelled)`.
- `crates/svn-wc/src/db/txn.rs`: `WcDb::with_txn(|t: &WcTxn| ...)` gives typed writers that all run in one `BEGIN IMMEDIATE` transaction. This covers the contract request for a single-transaction install of an updated file. The writers are:
  - reads: `read_base`, `conflict_skel`, `repository`
  - BASE: `base_add`, `base_remove`, `mark_incomplete`, `relocate_subtree`, `bump_revisions`
  - working layers: `convert_base_to_copy`, `mark_base_deleted`
  - ACTUAL_NODE: `set_actual_props`, `add_conflicts` (a new conflict replaces an existing one of the same kind and refreshes the operation)
  - stat cache and queue: `record_fileinfo`, `queue_work`
- `WcDb::moved_from(path)`.
- `wq_record_and_fetch_next` now wraps a crate-private raw variant; its behaviour is unchanged.

### svn-client uses only the facade

- Deleted `src/wc/adapters.rs` and `src/wc/txn.rs`. svn-client no longer depends on `rusqlite`; only the oracle test helpers still read raw rows to compare them with svn.
- The update editor, file install and finish code use `with_txn`. Conflicts are recorded with `WcTxn::add_conflicts`.
- `wc::repos_identity` now uses `read_repos_info`, and `wc::run_work_queue` is a thin wrapper.

### Bugs found by running the status/diff oracle tests (they had never run)

- diff: svn walks the tree depth first, with children in byte order and a directory's own property changes after its children. Ours printed rows in status order.
- diff: a deleted file did not print `Deleted: <prop>` sections.
- diff: a copied node (for example after an incoming delete over local edits) was compared with "nonexistent". It is now compared with its copy source and gets the `(revision N)` label.
- The test helper did not handle `--- Changelist` group headers, which caused a panic, and it hid property-only changes on the WC root.
- `percent_decode` could panic on a `%` followed by a multi-byte character.
- The `failpoints` feature did not compile: the macro used unimported `SvnError`/`ErrorCode`.
- Fixed the clippy findings: collapsible ifs, `== false`, `+ 0`, and too-many-arguments (allowed on two internal functions).

## Gates (all local; GitHub CI is skipped by the PO)

The real oracle is svn 1.15.0. The system svn is 1.14.3, which made every oracle test skip silently before this work. I built 1.15.0 with nix and ran the oracle tests with `OPENSVN_REQUIRE_SVN=1 OPENSVN_SVN_BIN=<svn 1.15.0>`.

| Gate | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --workspace --all-targets -- -D warnings` | pass |
| `cargo clippy -p svn-client -p svn-wc --all-targets --features failpoints -- -D warnings` | pass |
| `cargo test -p svn-client` (svn 1.15 required) | 35 passed, 0 failed: unit 8, fake checkout 3, fake local changes 8, oracle status/diff 4, oracle update 12 |
| `cargo test -p svn-client --features failpoints` | 35 passed, 0 failed |
| `cargo test -p svn-wc --features failpoints` (svn 1.15 required) | 214 passed, 0 failed, including 12 new `db_bindings` tests |

Not run: `cargo deny`, and `cargo test` for crates outside svn-client and svn-wc. Those crates still compile under the workspace clippy run.

## Still open (outside this PR's scope)

- There are no failpoint, cancellation or auth-order tests for update yet. The failpoint hooks exist and now compile.
- `file-commit` needs the commit code from PR #76, plugged in through `DbWorkingCopyOps::with_file_commit`.
- Diffs of deleted directories that have properties are not checked against svn yet.
