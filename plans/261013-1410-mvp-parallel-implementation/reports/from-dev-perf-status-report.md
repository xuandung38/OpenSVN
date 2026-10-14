# Working copy status at svn CLI speed (#121), PR #139

Branch `perf/status-121`. Scope: the local status path (`svn_client::status` /
`local_status`, used by the app's `wc_status`, commit and diff, and by
`compare-svn engine status`). Checkout and update were not touched.

## Result

`compare-svn --files 20000 --binaries 1 --binary-mb 16 --update-files 2000 --reps 3`, release
build, same 4-vCPU VM, apt svn 1.14.3 + Apache mod_dav_svn on 127.0.0.1 (median, min in brackets):

| status | svn CLI | engine before (main 9425158) | engine after |
|---|---:|---:|---:|
| first run, page cache dropped | 0.51 s / 0.37 s ¹ | 3.73 s | **0.17 s** (0.17) |
| repeat run (svn's checkout) | 0.14 s / 0.09 s ¹ | 3.19 s | **0.08 s** (0.05) |
| repeat run, own checkout | 0.09 s / 0.06 s ¹ | 2.96 s | **0.05 s** (0.05) |

¹ svn's time in the "before" run and in the "after" run (each bench run measures both tools).

The bench rounds to 10 ms and runs 3 times, so the last two rows look tight. More precise numbers come from
15 interleaved runs (median) on persistent working copies:

| working copy | svn | engine after |
|---|---:|---:|
| 20k files, svn checkout over HTTP (with `dav_cache`) | 0.056 s | 0.043 s |
| 20k files, engine checkout | 0.053 s | 0.035 s |
| 20k files, 2k modified (content compare needed) | 0.071 s | 0.044 s |
| 100k files (1000 dirs), svn checkout | 0.264 s | 0.173 s (main: 15.9–17.8 s) |

Checkout and update are still 8 to 25 times slower than svn. Another session owns them.

## Where the time went (perf + strace on main)

- About 12 SQLite statements per node, each prepared from scratch (`yy_reduce` and
  `sqlite3RunParser` were the top symbols), each in its own implicit read transaction. Every
  statement took and released the shared lock and probed for a hot journal (`fcntl`, ENOENT
  `stat`).
- `safe_join` per path `lstat`ed every ancestor directory (19 % after the database fix).
- One extra `lstat` per node in `disk_kind` and again in `text_modified`.
- A single thread.

## What changed

1. **svn-wc `WcDb::read_subtree`** (new, additive). It reads the NODES and ACTUAL_NODE rows of the
   target plus the LOCK cache in one read transaction. The rows are decoded by the existing
   `node_from_row`/`decode_props`. There is no `ORDER BY`: SQLite copied every row into a temp
   b-tree first. Rows are sorted in Rust only when they are not already in path order. For the
   whole working copy, the NODES read is split by rowid over `available_parallelism` read-only
   connections. The calling connection holds the read transaction and its shared lock meanwhile,
   so no writer can commit between parts. A part whose connection cannot open is read on the
   main connection.
2. **svn-client status**:
   - It indexes those rows (borrowed `&str` keys, no per-node allocation). It keeps the per-node
     decision code (`node_status`) unchanged, reading from the snapshot instead of from SQL.
   - Each directory is listed once, and its entries are `lstat`ed through `DirEntry::metadata`
     (`fstatat`, no symlink following).
   - Directories are listed on up to `available_parallelism` threads. Their items are then replayed
     depth-first, so the output order and the first error are those of the sequential walk.
   - `svn:externals` coverage is decided during that replay, in walk order, because svn's
     `is_external_path` depends on the definitions seen so far.
   - Rows are freed on a background thread.
3. **svn-types `safe_join_child`** (new, additive). It gives `safe_join`'s path and errors for a
   child of an already-joined directory without re-`lstat`ing the ancestors. A versioned
   directory replaced by a symlink is still refused with `UnsafePath`; a test checks this.
4. **`inspect::text_modified_stat`**. Same rule as before (and as svn): the size+mtime fast path,
   otherwise a SHA-1 compare of the repository-normal text, with eol-style, keywords and special
   files translated. `text_modified` now delegates to it.

## Semantics and self-review

- The fast path never reports a modified file as unmodified when its size or mtime differs.
  Mutation check: dropping the mtime condition makes both new oracle tests fail.
- A same-size edit whose mtime is restored to the recorded value is reported unmodified. svn
  1.15 does the same, and the test asserts equality with svn.
- Keyword files with an edited keyword value, eol-style files with foreign line endings, binary
  same-size edits, missing directories, obstructions, unversioned and ignored items, and depth
  `immediates`: all identical to svn 1.15 on an svn checkout and on an engine checkout.
- Format 31 compatibility: the status path only reads. Helper connections are read-only and
  hold no lock beyond SQLite's shared lock.
- One known difference: a corrupt row (unparsable skel or properties) anywhere in the target's
  subtree now fails the status run while the rows load. Before, it failed only when the walk
  reached that node.

## Gates (head `ef2f7e8` + report)

- `cargo fmt --all --check`: clean
- `cargo clippy --workspace --all-targets -- -D warnings`: clean
- `cargo clippy -p opensvn --features real-engine --all-targets -- -D warnings`: clean
- `cargo test --workspace --locked`: 1374 passed, 0 failed, 22 ignored
- `cargo test -p opensvn --features opensvn/real-engine`: 474 passed, 0 failed
- Oracle: svn 1.15.0 built from the repo's pinned recipe (`docker/compose.yml` version +
  SHA-512, `docker/svn-client/Dockerfile` configure flags), run with
  `OPENSVN_SVN_BIN=<1.15.0> OPENSVN_REQUIRE_SVN=1`. Nothing skipped. This includes
  `status_vs_svn`, `oracle_status_diff` and the new `status_fast_path_vs_svn`.

## Shared code touched (all additive)

- `svn-types`: `safe_join_child`.
- `svn-wc`: `db/subtree.rs` (`read_subtree`, `SubtreeRows`, `ActualRow`, `LockRow`).
- `svn-client`: status files plus `wc/inspect.rs` (`text_modified_stat`). Nothing in
  checkout/update or svn-ra.
