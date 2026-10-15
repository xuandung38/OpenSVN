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
