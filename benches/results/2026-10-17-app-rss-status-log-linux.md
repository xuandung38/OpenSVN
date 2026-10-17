<!-- Measured 2026-10-17 on a dev VM (branch perf/status-snapshots-log, AGENTS.md §5 items 1, 6, 18). -->
# App memory at refresh/update and History on 100k files (#21, #192)

- machine: 4 vCPU, 15 GB RAM, Linux 6.18.44, Ubuntu 24.04, Xvfb (no GPU: WebKit renders with llvmpipe)
- server: Apache 2.4.58 + mod_dav_svn 1.14.3 over HTTPS, Basic auth, mod_authz_svn
  (`scripts/test-env/visualsvn-like.sh up`, `/svn-basic`)
- oracle client: svn 1.15.0 (`docker/nix/subversion-1.15.nix`)
- app: release build (`pnpm tauri build --no-bundle --features real-engine`), default settings
  (hardware acceleration on, `MALLOC_ARENA_MAX=2` from #202)
- harness: `apps/opensvn/e2e/rss.test.mjs`, 100 000 files, 5 000-file watcher burst, 20 incoming
  revisions
- binaries: `main` is origin/main at edcb08f (#202, #203, #204, #205 merged); `branch` is this
  branch

## Result

| | main | branch |
|---|---:|---:|
| Outcome | stopped in History (no `Incoming 19` after 120 s) | **every step finished** |
| App RSS peak | 358.3 MB (update) | **318.6 MB** (update) |
| App heap at its peak | 195.8 MB | **156.4 MB** |
| Tree RSS peak | 817.8 MB | **804.5 MB** |
| Tree PSS peak | 648.6 MB | **633.6 MB** |

App RSS peak per step:

| Step | main | branch |
|---|---:|---:|
| checkout | 267.0 MB | 255.9 MB |
| status (cold reopen) | 279.1 MB | **248.4 MB** |
| refresh | 339.1 MB | **309.8 MB** |
| watcher burst | 330.4 MB | **309.8 MB** |
| update | 358.3 MB | **318.6 MB** |
| History | 333.3 MB (120 s, not finished) | **262.5 MB (2.5 s)** |

The test still fails its own 400 MB limit for the whole tree: the two WebKit processes take about
385 MB by themselves under llvmpipe (see `2026-10-16-app-rss-linux.md`). Of the app process,
150 MB is code (the binary, WebKit, and Mesa's LLVM for llvmpipe), so "app well under 250 MB"
is not reached under Xvfb; the heap part dropped by 40 MB at its peak.

## What changed

- **No second scan per status change.** `status_get` returns the published snapshot with an id;
  `wc://status-changed` carries `base`/`snapshot` ids and the UI applies the delta to the list it
  holds. Before, every delta made the UI call `status_get`, which scanned the whole tree again.
  `status_get` scans only when no scan of this app run produced the snapshot, and waits for one
  that is running.
- **One scan at a time.** Publications of a working copy run one after another, and a request
  that a later scan already answered does not scan (opening plus the first `status_get`, a
  refresh plus the watcher used to hold two full trees at once).
- **Properties decoded on demand.** `read_subtree` gives empty property lists to the row
  directly and keeps the undecoded list only for rows with properties (`LazyProps`); status reads
  `svn:special`, `svn:needs-lock`, `svn:mime-type` and emptiness without decoding lists that
  cannot hold them. A status scan of a 100k-file working copy (svn 1.15 checkout, every row with
  an empty list) peaks at the same 103 MB as main in a standalone run; a first version that kept
  a second vector of rows peaked at 161 MB.
- **Remote check walks only what the server named.** The check asks the server first and then
  looks at the nodes it named (`svn_client::status_remote_changes`, `local_status_of_paths`),
  instead of a `WcStatus` for every node (`show_unmodified: true`) filtered afterwards.

## History after an update (§5.18)

`svn log -q -l N https://…/svn-basic/<100k repo>/trunk` with svn 1.15 against mod_dav_svn 1.14.3
(authz on), 21 revisions, r1 the 100k-path import:

| limit | time |
|---:|---:|
| 16 | 0.11 s |
| 20 | 0.12 s |
| 21 | 178.9 s |
| 100 | 175.2 s |

The server checks authz for every changed path of each revision it reports and buffers the
report: nothing arrives before r1 is checked. Changes:

- A newest-first page is read in requests of 16, 32, … entries, each below the last entry of the
  one before; a request still running after 3 s with entries already in ends the page there
  (`nextRev` goes on below). The engine's own log of that repository: 20 entries (r21..r2) in
  3.1 s, `nextRev` r1.
- Identical log requests share one server read (`LogCache`, per location, range, limit, options),
  pages are kept until the working copy's revision moves or a remote check sees a newer HEAD.
- History reads its list without changed paths (`-v`); the paths of the selected revision (or of
  a compared range) are read when shown.
- A revision change cancels log reads still loading before invalidating them: TanStack Query
  otherwise joins the running fetch of a query that has no data yet, which was the read from
  start-up (HEAD r1, three minutes).
