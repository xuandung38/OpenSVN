# OpenSVN benchmarks

`compare-svn` measures the engine against the installed svn CLI. Both clients use the same
throwaway Apache + mod_dav_svn on `127.0.0.1`, which serves a generated repository.

```sh
sudo apt-get install subversion apache2 libapache2-mod-svn
cargo build --release -p opensvn-bench
target/release/compare-svn --files 100000 --binaries 4 --binary-mb 64 --update-files 10000 --reps 3
# report: target/bench-report/bench.md (+ bench.json)
```

Other installations: `OPENSVN_SVN_BIN` (svn and svnadmin), `OPENSVN_APACHE_BIN` and
`OPENSVN_APACHE_MODULES` (a directory with `mod_dav_svn.so` and the Apache modules), for instance
svn 1.15.0 and mod_dav_svn 1.15.0 built with nix.

The report records the svn CLI, the server (`Server:` header, which names mod_dav_svn's
version), the machine, the filesystem of the working copies (type, mount options, and whether it
journals, from `/proc/fs/jbd2` for ext3/ext4), and every repetition. Results worth keeping go to
`benches/results/<date>-<what>.md`.

- **Repository**: r1 = `--files` text files of about 1 to 3 KiB (100 folders x 10 subfolders) plus
  `--binaries` incompressible binaries of `--binary-mb` MiB. r2 changes `--update-files` text files
  and 1 MiB in the middle of one binary.
- **Checkout HEAD**: `svn checkout` vs `compare-svn engine checkout`, into an empty folder. Each
  repetition alternates which client goes first.
- **Update r1 -> r2**: each client on its own checkout. The client goes back to r1 untimed, then
  runs a timed update to r2.
- **Commit**: each client modifies `--commit-files` (default 1000) text files of its own
  checkout (its own files each repetition) and commits them; `svn status` must then be clean and
  `svn cat` must return a committed file's new content.
- **Status**:
  - first run: the page cache is dropped first when possible (root or passwordless sudo). The
    report says when it could not be dropped.
  - repeat runs: on the svn CLI's checkout, so both tools read the same input.
  - one extra row: each tool on its own checkout.

Each number is the wall time of a separate process (`svn ...` or `compare-svn engine ...`),
process start included. The median is reported, with the minimum next to it. An untimed
`svn status` / `svn info` follows every engine checkout and update, and the run aborts if the
working copy is not clean or not at the expected revision.

Release builds only: a debug build is marked "not representative" in the report.

The nightly workflow runs the 100k configuration. It emits a warning when an operation of the
engine is more than 10% slower than svn's.

## Commit

`compare-svn-commit` measures commits the same way, each client on its own repository and working
copy, with the bytes each client uploads counted by a pass-through proxy in front of Apache.

```sh
cargo build --release -p opensvn-bench --bin compare-svn-commit
target/release/compare-svn-commit --files 10000 --modify-files 1000 --big-mb 40 --reps 3
# report: target/bench-report/commit.md
```

- **add**: `--files` new text files of about 1 to 3 KiB in 100 folders, scheduled with `svn add`
  (untimed), then one timed commit.
- **modify**: `--modify-files` of those files get one more line, then one timed commit.
- **1 byte in a big file**: a `--big-mb` MiB random file is committed (untimed), one byte in the
  middle changes, then one timed commit.

After every engine commit an untimed `svn status` must be clean and `svn cat` of a changed file
must return the new content. `OPENSVN_BENCH_STDERR=1` passes the engine's stderr through.

The 1k-file commit benchmark is this tool with `--files 1000 --modify-files 1000`:

```sh
target/release/compare-svn-commit --files 1000 --modify-files 1000 --big-mb 40 --reps 3 \
  --out target/bench-report/commit-1k.md
```

Most of a commit of many files is server time (the `MERGE` of 10,000 added files alone takes 5 to
15 s on the same machine from one run to the next), so compare several repetitions, or subtract
the server time from Apache's access log (`%D`) to compare the clients' own work.

## Log

`compare-svn-log` measures `log` over a repository of `--revisions` revisions (default 100,000),
loaded with `svnadmin load` from a generated dump: r1 adds `--files` (default 100) small files,
every later revision changes one of them.

```sh
cargo build --release -p opensvn-bench --bin compare-svn-log
target/release/compare-svn-log --revisions 100000 --reps 3
# report: target/bench-report/log.md (+ log.json)
```

Rows: every revision (`svn log -q`), every revision with changed paths (`-v`), the newest 100
(`-l 100`), and the history of one file. Before each timed engine run, an untimed one must report
as many entries (and changed paths) as svn printed.

## Sync per file on macOS and Windows

On Linux the working copy writers skip the per-file `fsync` on local block filesystems and issue
one `syncfs` before the transaction that relies on the files (`crates/svn-wc/src/durable.rs`,
`BATCHED_SYNC`). On macOS and Windows every pristine and working file is still synced on its own,
so checkout and update there pay one `File::sync_all` per file: `fcntl(F_FULLFSYNC)` on macOS
(a full flush of the drive cache, much slower than a plain `fsync`) and `FlushFileBuffers` on
Windows. No numbers exist for those platforms yet. Before that behaviour
changes, measure on each: a 100k checkout and a 10k-file update on APFS (internal SSD) and NTFS
(internal SSD, Defender real-time scanning on and off), engine against svn 1.15, with per-file
sync as today and with the sync left out (a lower bound for any batching), and the share of the
time spent in sync calls (`fs_usage` / `dtruss` on macOS, ETW or Process Monitor on Windows).
