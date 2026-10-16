<!-- Measured 2026-10-15 on the dev VM of PR #192 (branch test/app-crash-rss-soak). -->
# Real app: crash recovery, RSS and soak (#20, #21)

- machine: 4 vCPU, 15 GB RAM, Linux 6.18.44, Ubuntu 24.04, Xvfb (no GPU: WebKit renders with llvmpipe)
- server: Apache 2.4.58 + mod_dav_svn 1.14.3 over HTTPS, Basic auth, mod_authz_svn
  (`scripts/test-env/visualsvn-like.sh up`, `/svn-basic`)
- oracle client: svn 1.15.0 (`docker/nix/subversion-1.15.nix`)
- app: release build with `real-engine` for RSS and soak; debug build with `failpoints` for the crash test
- memory: sum over the app process and every process below it (WebKitWebProcess,
  WebKitNetworkProcess), sampled every 250 ms (RSS) / 1 s (soak); PSS from `smaps_rollup`

## Crash e2e (`apps/opensvn/e2e/crash.test.mjs`)

SIGKILL of the app's whole process group, relaunch on the same profile, 240 files, 40-file commit,
update merging 80 incoming files into 35 local edits.

| Seed | Cases | Passed | Kills at a failpoint | Random kills | Commit landed before the kill |
|---|---:|---:|---:|---:|---:|
| 20261015 | 14 | 14 | 6 | 6 | 3 of 6 |
| 7 | 14 | 14 | 6 | 6 | 3 of 6 |

Every case: no repair banner, `svn status` clean or exactly the local edits, no duplicate commit,
the update finishes with every merge right. Before the fixes of #192 one case of 14 failed: killed
at `commit.after_finalize_before_queue`, the app came back without its working copy.

## RSS: app open, no working copy

| Configuration | Tree RSS | Tree PSS | App | WebKitWebProcess |
|---|---:|---:|---:|---:|
| default | 582 MB | 392 MB | 191 MB | 344 MB |
| `WEBKIT_DISABLE_COMPOSITING_MODE=1` | 417 MB | 250 MB | 184 MB | 186 MB |
| `WEBKIT_DISABLE_DMABUF_RENDERER=1` | 389 MB | 222 MB | 157 MB | 185 MB |
| `MALLOC_ARENA_MAX=2` | 580 MB | 389 MB | 191 MB | 342 MB |

WebKitNetworkProcess adds about 47 MB RSS (default run).

## RSS: 100k-file working copy (`apps/opensvn/e2e/rss.test.mjs`)

100,000 text files in 1,000 folders. Checkout through the UI, then a restart and a cold status.

| Configuration | Checkout | Peak tree RSS | Peak tree PSS | Peak app RSS | Status after restart: tree RSS |
|---|---:|---:|---:|---:|---:|
| default | 185 s | 824 MB | 636 MB | 310 MB | 635 MB |
| `WEBKIT_DISABLE_DMABUF_RENDERER=1` | 190 s | 585 MB | 419 MB | 282 MB | 462 MB |

- The 400 MB limit for the process tree does not hold, not even at idle. Without the
  DMA-BUF renderer the app process alone peaks at 282 MB during the 100k checkout.
- The remaining steps never ran:
  - The refresh found the working copy busy for the whole 300 s allowed (112 refused attempts),
    in both configurations.
  - Cause: the History sidebar's `log_get` holds a read of the working copy for its whole network
    call. On this server, a log of the 100k-path import revision takes minutes. The start-up open
    queues a write behind it, and from then on every read is refused.
  - 20k files, same cause: busy for 30 s. After that, a refresh takes 0.2 s.

## Soak: 10 min, 50k files (`apps/opensvn/e2e/soak.test.mjs`)

Watcher and poller (1 min), 50 edits every 15 s, another user's commit every minute, an update
every 2 minutes.

| App environment | Peak tree RSS | Peak app RSS | Mean after warm-up | Mean, last fifth | Growth |
|---|---:|---:|---:|---:|---:|
| default (glibc) | 763 MB | 263 MB | 726 MB | 734 MB | +8.0 MB |
| `MALLOC_ARENA_MAX=2` | 778 MB | 271 MB | 747 MB | 751 MB | +3.9 MB |
| jemalloc (`LD_PRELOAD`) | 791 MB | 298 MB | 723 MB | 711 MB | −11.8 MB |

- **No memory creep in 10 minutes.** The app process stays flat (262–263 MB with glibc). With
  jemalloc it gives memory back over time (298 → 219 MB) but peaks higher.
  `MALLOC_ARENA_MAX=2` changes nothing measurable. No allocator change is warranted by these
  numbers.
- **None of the 4 periodic updates per run started.** All stayed queued ("4 running"). The same
  `log_get` cause applies: the sidebar's log of the 50k-path import takes 86 s on the server and is
  requested again every minute, so the working copy is always read-held and the update (a write)
  never gets its turn.

## Engine soak `soak_short` (`tests/crash/tests/soak.rs`, 2026-10-16)

Reported failing on main 45210f1 (RSS +39%); reproduced here with main 662b6ef merged in ("main"
rows). Same VM, release build, 2000 files, 40 changed per cycle by each side.

| Run | Allocator | Warm-up ends | RSS after warm-up | RSS at the end | Growth |
|---|---|---|---:|---:|---:|
| main, 2 min | glibc default | cycle 19 | 23.6 MB | 31.4 MB | **+32.9%** (fail) |
| main, 10 min | glibc default | cycle 85 | 29.7 MB | 33.2 MB | **+11.7%** (fail) |
| main, 2 min | `MALLOC_ARENA_MAX=2` | cycle ~18 | | 22.6 MB | +7.3% |
| main, 2 min | `MALLOC_ARENA_MAX=1` | cycle ~18 | | 20.9 MB | +5.5% |
| main, 2 min | jemalloc (`LD_PRELOAD`) | cycle 14 | 25.7 MB | 27.3 MB | +5.9% |
| fixed, 2 min | `MALLOC_ARENA_MAX=2` | cycle 50 | 20.9 MB | 21.9 MB | +5.0% |
| fixed, 10 min | `MALLOC_ARENA_MAX=2` | cycle 89 | 21.3 MB | 22.5 MB | +5.4% (flat from 380 s) |

- **Not a leak.** heaptrack over 5 minutes: the live heap returns to about 1.24 MiB after every cycle
  (2–2.7 MiB at the peak of a cycle) from the first minute to the last.
- The RSS growth came from two things:
  - **glibc arenas.** Freed pages stay in the arenas of tokio's worker and blocking threads. Bounding
    the arenas, or switching to jemalloc, removes most of the growth.
  - **The warm-up ended too early.** The log of the newest 100 entries with changed paths only
    reaches 100 entries at cycle 50, so until then each cycle legitimately reads more history.
- **Fix (test only):**
  - the warm-up also waits for a full log window;
  - the test starts itself again under `MALLOC_ARENA_MAX=2` when the variable is unset;
  - the report names the allocator setting.
- **Real app:** `MALLOC_ARENA_MAX=2` changed nothing measurable in the 10 min app soak above
  (+3.9 MB vs +8.0 MB growth, peaks within 15 MB). The engine needs no allocator change.
