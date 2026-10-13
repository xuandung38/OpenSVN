/** rAF-based frame-time meter used by the dev-only WebView perf spike. */

export interface FrameStats {
  frames: number;
  durationMs: number;
  avgFps: number;
  p50Ms: number;
  p95Ms: number;
  p99Ms: number;
  maxMs: number;
  /** Frames slower than 20ms (below ~50 fps), as a percentage of all frames. */
  slowFramePct: number;
}

function pct(sorted: number[], p: number): number {
  if (sorted.length === 0) return 0;
  const i = Math.min(sorted.length - 1, Math.floor((p / 100) * sorted.length));
  return sorted[i] ?? 0;
}

export function summarize(deltas: number[], durationMs: number): FrameStats {
  const sorted = [...deltas].sort((a, b) => a - b);
  const slow = deltas.filter((d) => d > 20).length;
  return {
    frames: deltas.length,
    durationMs,
    avgFps: durationMs > 0 ? (deltas.length / durationMs) * 1000 : 0,
    p50Ms: pct(sorted, 50),
    p95Ms: pct(sorted, 95),
    p99Ms: pct(sorted, 99),
    maxMs: sorted[sorted.length - 1] ?? 0,
    slowFramePct: deltas.length > 0 ? (slow / deltas.length) * 100 : 0,
  };
}

/**
 * Runs `step(tMs)` every animation frame for `durationMs` and records frame-to-frame deltas.
 * `step` drives the scripted scroll. Resolves with the summary when finished.
 */
export function measureFrames(
  durationMs: number,
  step: (elapsedMs: number) => void,
  onFrame?: (deltaMs: number) => void,
): Promise<FrameStats> {
  return new Promise((resolve) => {
    const deltas: number[] = [];
    let start = 0;
    let last = 0;
    const tick = (now: number) => {
      if (start === 0) {
        start = now;
        last = now;
      } else {
        const d = now - last;
        deltas.push(d);
        onFrame?.(d);
        last = now;
      }
      const elapsed = now - start;
      if (elapsed >= durationMs) {
        resolve(summarize(deltas, elapsed));
        return;
      }
      step(elapsed);
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  });
}

interface ChromiumMemory {
  usedJSHeapSize: number;
  totalJSHeapSize: number;
}

/** JS heap in MB where `performance.memory` exists (Chromium/WebView2); null on WebKit. */
export function heapMb(): number | null {
  const mem = (performance as unknown as { memory?: ChromiumMemory }).memory;
  return mem ? Math.round((mem.usedJSHeapSize / 1048576) * 10) / 10 : null;
}
