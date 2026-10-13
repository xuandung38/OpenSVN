# P09 WebView perf spike report (ADR-002 gate)

Date: 2026-10-13 · Author: dev-p09 · PR: feat/p09-fe-shell-m1 (issue #12)
Status: macOS measured. Linux (Ubuntu 24.04 / Fedora) and Windows 11 pending (manual steps below).

## What is measured
Dev-only route `/__perf` (code: `apps/opensvn/src/perf/`):
- 100k virtualised `FileRow`s (TanStack Virtual, row 28px, overscan 12) with scripted scroll (top->bottom->top) at 3000 / 6000 / 12000 px/s, 5s each (12000 px/s is about 430 rows/s, faster than real flicks).
- Baseline: same scroll (6000 px/s) over a plain pre-rendered 3000-row `<div>` (no React, no virtualisation). Separates WebView/compositor/machine limits from app cost.
- CodeMirror 6 `@codemirror/merge` `MergeView`, 20,000-line doc, ~3% lines changed. "Diff open" = construct view (includes diff computation) until 2nd animation frame. Then 5s scripted scroll at 6000 px/s.
- Meter: rAF frame deltas -> avg fps, p50/p95/p99/max frame ms, % frames > 20ms. Heap via `performance.memory` where present (Chromium/WebView2 only; WKWebView/WebKitGTK report `null`).

Measured in a **release-mode frontend bundle** (`vite build --mode perf` + `vite preview`) inside the real Tauri shell (debug Rust binary, which is irrelevant to WebView cost). Results are posted by the page to the preview server and printed to the terminal as `[perf-report] {json}`.

## Pass criteria (from phase-09 + ADR-002)
- List scrolling: ~60 fps. Operational definition used here: avg >= 55 fps at <= 6000 px/s, p50 <= 17ms, and within ~5% of the plain-div baseline on the same machine.
- Diff of 20k lines opens < 300 ms.
Note: WebKit rounds rAF timestamps to whole ms, so a perfect 60 Hz shows p50 = 17 ms.

## macOS results (WKWebView)
Machine: MacBook, Apple M1 Pro, 32 GB, macOS 27.0.1, display DPR 2 (60 Hz), Tauri 2.12 / WKWebView (system). Machine was **under load**: about 9 other agents were compiling Rust in parallel during the run, so the numbers are pessimistic and noisy (the baseline itself shows 16% slow frames).

Run 1 (release bundle, final instrumented run):

| Test | avg fps | p50 ms | p95 ms | p99 ms | max ms | frames > 20ms |
|---|---|---|---|---|---|---|
| List 100k @ 3000 px/s | 58.0 | 17 | 29 | 44 | 48 | 23% |
| List 100k @ 6000 px/s | 57.6 | 17 | 32 | 42 | 53 | 21% |
| List 100k @ 12000 px/s | 53.3 | 17 | 32 | 52 | 83 | 30% |
| Baseline plain div @ 6000 px/s | 59.6 | 17 | 25 | 34 | 42 | 16% |
| Diff 20k lines scroll @ 6000 px/s | 58.0 | 17 | 26 | 38 | 106 | 17% |

| Test | Result |
|---|---|
| Diff open (20k lines, MergeView) | **135 ms** |
| JS heap | n/a (WKWebView has no `performance.memory`) |

Earlier runs, for variance: dev bundle (unminified React + StrictMode): list 12000 px/s 56.7 fps, p95 31 ms, diff open 184 ms, diff scroll 60.0 fps. Release bundle before baseline was added: list 12000 px/s 53.6 fps, diff open 101 ms, diff scroll 59.2 fps. Diff open: 101-184 ms across runs.

### Verdict macOS
- Diff open < 300 ms: **PASS** (101-184 ms, 135 ms in final run).
- List 60 fps: **PASS with caveat**. At realistic speeds (<= 6000 px/s) avg 57.6-58.0 fps vs 59.6 for a plain non-virtualised div, so the virtualised list costs about 2 fps over the machine floor; p50 is 17 ms (vsync) in every run. The 12000 px/s stress (53 fps) is below the bar but is 2-4x faster than interactive scrolling. Slow frames are dominated by machine load (baseline also has 16%), not by the list. Recommend re-measuring on an idle machine; expect closer to 60.
- Memory: not measurable on WKWebView; check on Windows (WebView2 exposes `performance.memory`) and use Activity Monitor "WebContent" RSS on macOS.
- Decision input for ADR-002 on macOS: **keep Tauri** (no WebView blocker).

## Linux and Windows: manual measurement steps
Prereqs (all OS): Node >= 24, pnpm 11 (`corepack enable`), Rust toolchain per `rust-toolchain.toml`, repo checked out on branch `feat/p09-fe-shell-m1`, then `pnpm install` at repo root. Close other heavy apps; plug in laptop power; use the display at its native 60 Hz (or note the refresh rate).

Run (from repo root, any OS):
```
cd apps/opensvn
pnpm perf:spike
```
This builds the frontend with `--mode perf` (keeps `/__perf` in the bundle), serves it with `vite preview` on :1420 and opens the Tauri window, which auto-runs the script after about 1.5 s (about 35 s total: 3x5 s list, 5 s baseline, diff open, 5 s diff scroll). The JSON result is printed in the terminal as a `[perf-report]` line and shown at the bottom of the window. Copy the final line (the one with `"list"`) into the PR / this report. Re-run the whole thing 3 times and keep the median run. The window must stay visible and un-occluded (WebViews pause rAF when hidden; a hidden window makes the run stall with no result). The "Run script" button reruns it manually.

Linux (WebKitGTK):
1. Ubuntu 24.04: `sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev`. Fedora: `sudo dnf install webkit2gtk4.1-devel openssl-devel curl wget file libxdo-devel libappindicator-gtk3-devel librsvg2-devel gcc-c++`.
2. Run the 3x above on each of: Intel iGPU, AMD GPU, NVIDIA (proprietary driver). Record `glxinfo -B | head -20` (or `lspci -k | grep -A3 VGA`), session type (X11 vs Wayland: `echo $XDG_SESSION_TYPE`) and `pkg-config --modversion webkit2gtk-4.1`.
3. If the window is blank or very slow on NVIDIA/Wayland, rerun with `WEBKIT_DISABLE_COMPOSITING_MODE=1 pnpm perf:spike` and, separately, `WEBKIT_DISABLE_DMABUF_RENDERER=1 pnpm perf:spike`; record which env var (if any) is needed. This is the known WebKitGTK failure mode and decides whether Tauri is viable on Linux.
4. Record the numbers in a table like the one above, one row per machine/GPU/session type.

Windows 11 (WebView2):
1. Install Visual Studio Build Tools (C++ workload) and the WebView2 runtime (preinstalled on Win 11). PowerShell: `cd apps\opensvn; pnpm perf:spike`.
2. WebView2 is Chromium, so `performance.memory` works: the `listHeapMb` / `diffHeapMb` fields in the JSON give JS heap in MB. Also record Task Manager "Details" -> `msedgewebview2.exe` (sum of the opensvn child processes) working set after the run.
3. Note GPU, driver, and display refresh rate (Settings -> Display -> Advanced).

What to report per machine: OS + version, CPU/GPU, WebView version (WKWebView = macOS version; WebKitGTK = pkg-config version; WebView2 = `edge://version`-equivalent from the runtime), refresh rate, the JSON, and a PASS/FAIL line per criterion above. Decision rule for ADR-002: if all three OS pass the criteria, keep Tauri. If Linux fails only on some GPU/driver combos and an env var or setting fixes it, keep Tauri and document the workaround; if a common combo (Intel/AMD on Ubuntu 24.04) cannot reach about 50 fps on the 6000 px/s list or opens the diff in > 300 ms, escalate to the lead to reconsider Electron (only the shell changes, the FE talks to an IPC adapter).

## Notes found while building the spike
- CodeMirror (style-mod) injects styles via constructed stylesheets when supported; under production CSP `style-src 'self'` this must be re-verified in the real WKWebView/WebKitGTK/WebView2 when DiffView ships in Milestone 2 (the spike ran under `devCsp`, which allows `'unsafe-inline'`). The `/__perf` route is DEV-only (or `--mode perf`), so the shipped CSP is unaffected today.
- `performance.memory` is Chromium-only; WebKit gives `null`.
