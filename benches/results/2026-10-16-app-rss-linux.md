<!-- Measured 2026-10-16 on a dev VM (branch test/finish-21, #21). -->
# App memory on Linux: what fills it, allocator, renderer (#21)

- machine: 4 vCPU, 15 GB RAM, Linux 6.18.44, Ubuntu 24.04, Xvfb (no GPU: WebKit renders with llvmpipe)
- server: Apache 2.4.58 + mod_dav_svn 1.14.3 over HTTPS, Basic auth, mod_authz_svn
  (`scripts/test-env/visualsvn-like.sh up`, `/svn-basic`)
- oracle client: svn 1.15.0 (`docker/nix/subversion-1.15.nix`)
- app: release build (`pnpm tauri build --no-bundle --features real-engine`)
- harness: `apps/opensvn/e2e/rss.test.mjs`. Memory is the sum over the app process and every process
  below it (WebKitWebProcess, WebKitNetworkProcess), sampled every 250 ms. The report now also
  splits the app process at its peak by mapping: heap (brk heap, anonymous mappings: malloc
  arenas, stacks), gpu (`/dev/dri`, memfd, shared memory) and code (binary, libraries).
- binaries: `main` is origin/main at 85f09fa + #200 (`log_get` outside the working copy actor), so
  every run gets past the refresh; `branch` is this branch.

Every 100k run stops in the History step: the log of the 100k-path import revision takes longer
than the 120 s the test waits under authz (AGENTS.md §5.18). The steps before it all finish.

## What the app process holds

App open, no working copy, `main`, smaps of the app process:

| Mapping | RSS |
|---|---:|
| `libLLVM.so.20.1` (Mesa llvmpipe) | 54.0 MB |
| `libwebkit2gtk-4.1.so` | 24.1 MB |
| heap (`[heap]`) | 18.7 MB |
| the `opensvn` binary | 16.6 MB |
| `libjavascriptcoregtk-4.1.so` | 12.3 MB |
| `libgallium` (Mesa llvmpipe) | 9.2 MB |
| WebKit shared memory, SYSV | 11.4 MB |
| anonymous | 5.4 MB |
| everything else | about 33 MB |
| **total** | **185 MB** |

The web process holds another 345 MB (140 MB of anonymous llvmpipe/GL memory, LLVM 58 MB,
WebKit 55 MB, JSC 21 MB). The same libraries count in both processes' RSS; PSS counts them once.

During a 30k-file checkout the app's heap mapping reached 78 MB, but heaptrack (attached to the
running app) saw a **live heap peak of 22.9 MB**, 14.6 MB of it SQLite page caches. The rest was
memory freed by threads (tokio workers, the blocking pool, the status scan threads) and kept by
glibc in per-thread arenas.

## Allocator (30k files, `main`)

The allocator variables reach the WebKit processes too (inherited environment).

| App environment | Tree RSS peak | Tree PSS peak | App RSS peak | App heap at its peak |
|---|---:|---:|---:|---:|
| glibc default | 873 MB | 685 MB | 372 MB | 207 MB |
| `MALLOC_ARENA_MAX=2` | **763 MB** | **585 MB** | **247 MB** | **83 MB** |
| jemalloc (`LD_PRELOAD`) | 808 MB | 627 MB | 292 MB | 128 MB |
| mimalloc (`LD_PRELOAD`) | 806 MB | 626 MB | 284 MB | 119 MB |

App RSS peak by step:

| Step | glibc | `MALLOC_ARENA_MAX=2` | jemalloc | mimalloc |
|---|---:|---:|---:|---:|
| checkout | 253 MB | 231 MB | 258 MB | 247 MB |
| status (cold reopen) | 244 MB | 218 MB | 248 MB | 238 MB |
| refresh | 284 MB | 242 MB | 285 MB | 266 MB |
| watcher burst (5k files) | 329 MB | 237 MB | 292 MB | 271 MB |
| update (20 revisions) | 360 MB | 247 MB | 282 MB | 284 MB |
| history (stopped) | 372 MB | 239 MB | 282 MB | 271 MB |

With glibc's default the app grows with every step; with two arenas it stays flat after the
checkout. The app now starts itself with `MALLOC_ARENA_MAX=2` (`src-tauri/src/startup_env.rs`:
glibc reads it only at process start and the crate forbids `unsafe`, so the process execs itself
once with the variable, same pid, before any thread).

## Hardware acceleration (app open, no working copy, `branch`)

| Configuration | App | Web process | Network process | Tree RSS | Tree PSS |
|---|---:|---:|---:|---:|---:|
| default (acceleration on) | 187 MB | 337 MB | 47 MB | 572 MB | 392 MB |
| `OPENSVN_HARDWARE_ACCELERATION=0` | 153 MB | 181 MB | 47 MB | **383 MB** | **227 MB** |

Off sets `WEBKIT_DISABLE_DMABUF_RENDERER=1` and `WEBKIT_DISABLE_COMPOSITING_MODE=1` at start-up:
no GL in either process, so no llvmpipe. A first version set WebKit's hardware acceleration policy
to "never" after the web view existed: the web process dropped to 218 MB, but the app process kept
its GL setup (189 MB, tree 456 MB). These numbers are for a machine without a GPU driver. With a
GPU the renderer uses the driver instead of llvmpipe; that has to be measured on real hardware
before the Linux default changes (#182).

## 100k files

`branch` ran twice with acceleration on (peaks within 7 MB of each other); the table shows the
second run, made on an otherwise idle machine like the other two columns.

| Step | `main`: app | `branch`: app | `branch`, acceleration off: app | `main`: tree | `branch`: tree | `branch`, acceleration off: tree |
|---|---:|---:|---:|---:|---:|---:|
| checkout | 305 MB | 260 MB | 238 MB | 787 MB | 744 MB | 542 MB |
| status (cold reopen) | 302 MB | 282 MB | 262 MB | 673 MB | 656 MB | 498 MB |
| refresh | 444 MB | 349 MB | 316 MB | 831 MB | 732 MB | 560 MB |
| watcher burst (5k files) | 483 MB | 306 MB | 323 MB | 889 MB | 713 MB | 589 MB |
| update (20 revisions) | **594 MB** | 342 MB | **388 MB** | **1041 MB** | **804 MB** | **722 MB** |
| history (stopped) | 573 MB | 322 MB | 332 MB | 1033 MB | 787 MB | 622 MB |
| **peak** | **594 MB** | **349 MB** | **388 MB** | **1041 MB** | **804 MB** | **722 MB** |

PSS of the tree at its peak: `main` 866 MB, `branch` 628 MB, `branch` with acceleration off
560 MB. With acceleration off the app process itself holds more at the update (388 MB, 239 MB of
it anonymous memory against 188 MB with acceleration on): the UI process then paints the page in
software, and its buffers are anonymous mappings too.

Before #200 (`main` at 85f09fa, the measurement of #192 repeated here) the run stopped in the
refresh step: app 324 MB, tree 820 MB at the checkout.

## What is left

- The tree stays above 400 MB under Xvfb: the two WebKit processes alone hold about 385 MB RSS
  with acceleration on. With it off the app at rest is at 383 MB.
- The app process still climbs to 350 MB at refresh and update: several whole-tree status scans
  run one after another (`wc://status-changed` makes the UI call `status_get`, another scan),
  each decoding every NODES row with its properties (`read_subtree`). AGENTS.md §5.6 lists them.
