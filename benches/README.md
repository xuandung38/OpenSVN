# OpenSVN benchmarks

`compare-svn` measures the engine against the installed svn CLI. Both clients use the same
throwaway Apache + mod_dav_svn on `127.0.0.1`, which serves a generated repository.

```sh
sudo apt-get install subversion apache2 libapache2-mod-svn
cargo build --release -p opensvn-bench
target/release/compare-svn --files 100000 --binaries 4 --binary-mb 64 --update-files 10000 --reps 3
# report: target/bench-report/bench.md (+ bench.json)
```

- **Repository**: r1 = `--files` text files of about 1 to 3 KiB (100 folders x 10 subfolders) plus
  `--binaries` incompressible binaries of `--binary-mb` MiB. r2 changes `--update-files` text files
  and 1 MiB in the middle of one binary.
- **Checkout HEAD**: `svn checkout` vs `compare-svn engine checkout`, into an empty folder. Each
  repetition alternates which client goes first.
- **Update r1 -> r2**: each client on its own checkout. The client goes back to r1 untimed, then
  runs a timed update to r2.
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
