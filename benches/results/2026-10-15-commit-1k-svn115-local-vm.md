<!-- Measured 2026-10-15 on the dev VM of PR #170 (release build), svn 1.15.0 CLI and mod_dav_svn 1.15.0 built with nix, Apache 2.4.69. Three runs of `compare-svn-commit --files 1000 --modify-files 1000 --big-mb 40`; the third ran every timed command under OPENSVN_BENCH_WRAP to read its CPU time. -->
# Benchmark: commit of 1,000 files, OpenSVN engine vs svn CLI

- svn CLI 1.15.0; server: Apache/2.4.69 (Unix) SVN/1.15.0, HTTP on 127.0.0.1
- machine: Intel(R) Xeon(R) Processor, 4 CPUs, 15.7 GiB RAM, Linux 6.18.44-fc-v114
- filesystem of the working copies: ext4 on /dev/vda (rw,relatime,discard,no_prefetch_block_bitmaps,resv_strict,resuid=65534,resgid=65534); NOT journaled (no jbd2 journal for this device)
- build: release, repetitions: 5 (median, min in brackets)
- bytes: what the client sent to the server during the commit (median)

## Run 2 (5 repetitions)

| operation | svn CLI | engine | engine / svn | svn bytes | engine bytes |
|---|---:|---:|---:|---:|---:|
| commit: add 1000 files | 4.05 s (3.36) | 3.65 s (3.22) | 0.90 | 1601102 | 1621392 |
| commit: modify 1000 files | 3.51 s (3.17) | 4.00 s (3.78) | 1.14 | 541183 | 561562 |
| commit: 1 byte changed in a 40 MiB file | 0.98 s (0.80) | 0.87 s (0.75) | 0.89 | 10465 | 10864 |

### Every repetition

| operation | svn CLI | engine |
|---|---|---|
| commit: add 1000 files | 3.81, 4.59, 4.29, 3.36, 4.05 | 3.65, 4.30, 4.42, 3.22, 3.50 |
| commit: modify 1000 files | 3.51, 4.55, 4.38, 3.17, 3.45 | 3.78, 4.62, 4.61, 4.00, 3.80 |
| commit: 1 byte changed in a 40 MiB file | 0.80, 0.98, 1.01, 0.89, 1.03 | 1.06, 0.87, 1.08, 0.75, 0.82 |

## Run 3 (5 repetitions, CPU time recorded)

| operation | svn CLI | engine | engine / svn | svn bytes | engine bytes |
|---|---:|---:|---:|---:|---:|
| commit: add 1000 files | 4.05 s (3.96) | 4.39 s (4.05) | 1.08 | 1601102 | 1621392 |
| commit: modify 1000 files | 4.56 s (3.85) | 4.23 s (3.88) | 0.93 | 541183 | 561562 |
| commit: 1 byte changed in a 40 MiB file | 1.07 s (1.03) | 0.90 s (0.68) | 0.84 | 10465 | 10864 |

### Every repetition

| operation | svn CLI | engine |
|---|---|---|
| commit: add 1000 files | 3.96, 4.00, 4.70, 4.05, 4.13 | 4.24, 4.52, 4.59, 4.05, 4.39 |
| commit: modify 1000 files | 3.85, 4.56, 4.69, 4.55, 4.69 | 4.23, 4.47, 4.47, 3.98, 3.88 |
| commit: 1 byte changed in a 40 MiB file | 1.07, 1.08, 1.03, 1.03, 1.08 | 1.00, 0.92, 0.90, 0.74, 0.68 |

## Run 1 (3 repetitions, medians)

| operation | svn CLI | engine | engine / svn |
|---|---:|---:|---:|
| commit: add 1000 files | 4.06 s (3.72) | 3.37 s (3.11) | 0.83 |
| commit: modify 1000 files | 3.45 s (3.13) | 3.39 s (3.17) | 0.98 |
| commit: 1 byte changed in a 40 MiB file | 0.97 s (0.93) | 0.99 s (0.93) | 1.02 |

## Client CPU (run 3, user + system seconds per process)

| operation | svn CLI | engine |
|---|---|---|
| add 1000 files | 0.73, 0.81, 1.08, 0.84, 0.86 | 1.05, 1.11, 1.26, 0.96, 1.04 |
| modify 1000 files | 0.64, 0.73, 0.95, 0.86, 0.90 | 0.98, 0.83, 0.91, 0.77, 0.81 |
| 1 byte in a 40 MiB file | 0.64, 0.60, 0.56, 0.56, 0.58 | 0.60, 0.53, 0.45, 0.37, 0.31 |

## Notes

- Across the three runs the median ratio engine / svn is 0.83, 0.90, 1.08 for the add, 0.98,
  1.14, 0.93 for the modify and 1.02, 0.89, 0.84 for the one-byte change: within the run-to-run
  spread of either client (up to 1.4 s on the same commit), neither consistently ahead.
- Most of a 1,000-file commit is server time (wall 3.2 to 4.7 s against 0.6 to 1.3 s of client
  CPU). The engine's own work is about 0.2 s more than svn's for 1,000 added files, nearly all of
  it system time; for modified files the two are level. Both send the same bytes within 4%.
- The one-byte change of a 40 MiB file sends a delta (10 KiB) in both clients (#132).
