import { useCallback, useMemo, useRef, useState } from "react";
import { FileRow } from "../components/list/file-row";
import { VirtualList } from "../components/list/virtual-list";
import { createMergeView } from "./diff-perf-view";
import { type FrameStats, heapMb, measureFrames } from "./frame-meter";
import { makeDiffDocs, makeRows } from "./perf-data";

const ROW_H = 28;
const LIST_ROWS = 100_000;
const DIFF_LINES = 20_000;
const SCROLL_MS = 5000;
const LIST_SPEEDS = [3000, 6000, 12000];
const BASELINE_SPEED = 6000;

interface PerfResult {
  userAgent: string;
  devicePixelRatio: number;
  /** Virtualised list scripted scroll at several speeds (px/s). */
  list: { pxPerSec: number; stats: FrameStats }[];
  /** Same scroll speed over a plain pre-rendered 3000-row div: separates WebView limits from app cost. */
  baseline: FrameStats | null;
  listHeapMb: number | null;
  diffOpenMs: number | null;
  diffScroll: FrameStats | null;
  diffHeapMb: number | null;
}

/** Dev-only spike page: 100k virtualised rows + 20k-line CodeMirror merge view with scripted scroll. */
export function PerfPage() {
  const rows = useMemo(() => makeRows(LIST_ROWS), []);
  const listRef = useRef<HTMLDivElement>(null);
  const diffRef = useRef<HTMLDivElement>(null);
  const meterRef = useRef<HTMLSpanElement>(null);
  const [status, setStatus] = useState("idle");
  const [result, setResult] = useState<PerfResult | null>(null);

  // Smooth live readout (exponential moving average) without re-rendering every frame.
  const ema = useRef(16.7);
  const lastPaint = useRef(0);
  const onFrame = useCallback((d: number) => {
    ema.current = ema.current * 0.9 + d * 0.1;
    const now = performance.now();
    if (now - lastPaint.current > 250 && meterRef.current) {
      lastPaint.current = now;
      // Written straight to the DOM: re-rendering React per update would perturb the measurement.
      meterRef.current.textContent = `${(1000 / ema.current).toFixed(0)} fps · ${ema.current.toFixed(1)} ms/frame · heap ${heapMb() ?? "n/a"} MB`;
    }
  }, []);

  /** Sweeps an element top -> bottom -> top at `pxPerSec` for `ms` while measuring frames. */
  const scrollScript = useCallback(
    async (el: HTMLElement, pxPerSec: number, ms: number) => {
      el.scrollTop = 0;
      await new Promise((r) => requestAnimationFrame(() => r(null)));
      const total = Math.max(1, el.scrollHeight - el.clientHeight);
      return measureFrames(
        ms,
        (t) => {
          const phase = (((t * pxPerSec) / 1000) % (total * 2)) / total;
          el.scrollTop = (phase <= 1 ? phase : 2 - phase) * total;
        },
        onFrame,
      );
    },
    [onFrame],
  );

  const runList = useCallback(async () => {
    const el = listRef.current;
    if (!el) throw new Error("list not mounted");
    const runs: PerfResult["list"] = [];
    for (const pxPerSec of LIST_SPEEDS) {
      runs.push({ pxPerSec, stats: await scrollScript(el, pxPerSec, SCROLL_MS) });
    }
    return runs;
  }, [scrollScript]);

  const runBaseline = useCallback(async () => {
    const host = diffRef.current;
    if (!host) throw new Error("diff host not mounted");
    const box = document.createElement("div");
    box.className = "h-full overflow-auto font-mono text-[12px]";
    for (let i = 0; i < 3000; i++) {
      const row = document.createElement("div");
      row.className = "px-2.5 py-[5px]";
      row.textContent = `plain-row-${i} src/some/dir/file-${i}.ts`;
      box.appendChild(row);
    }
    host.replaceChildren(box);
    return scrollScript(box, BASELINE_SPEED, SCROLL_MS);
  }, [scrollScript]);

  const runDiff = useCallback(async () => {
    const host = diffRef.current;
    if (!host) throw new Error("diff host not mounted");
    host.replaceChildren();
    const [a, b] = makeDiffDocs(DIFF_LINES);
    // Open time = construct merge view (includes diff computation) until first painted frame.
    const t0 = performance.now();
    const view = createMergeView(host, a, b);
    await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(() => r(null))));
    const openMs = performance.now() - t0;
    const scroller = view.b.scrollDOM;
    const stats = await scrollScript(scroller, BASELINE_SPEED, SCROLL_MS);
    return { openMs, stats };
  }, [scrollScript]);

  const runAll = useCallback(async () => {
    const stage = (name: string) => {
      setStatus(name);
      fetch("/__perf-report", { method: "POST", body: JSON.stringify({ stage: name }) }).catch(
        () => undefined,
      );
    };
    try {
      stage("running list scroll");
      const list = await runList();
      const listHeapMb = heapMb();
      stage("running baseline plain-div scroll");
      const baseline = await runBaseline();
      stage("running diff open + scroll");
      const { openMs, stats } = await runDiff();
      const r: PerfResult = {
        userAgent: navigator.userAgent,
        devicePixelRatio: window.devicePixelRatio,
        list,
        baseline,
        listHeapMb,
        diffOpenMs: Math.round(openMs),
        diffScroll: stats,
        diffHeapMb: heapMb(),
      };
      setResult(r);
      setStatus("done");
      // Dev server middleware prints the report; ignore failures (page works standalone).
      fetch("/__perf-report", { method: "POST", body: JSON.stringify(r) }).catch(() => undefined);
    } catch (e) {
      setStatus(`error: ${String(e)}`);
      fetch("/__perf-report", { method: "POST", body: JSON.stringify({ error: String(e) }) }).catch(
        () => undefined,
      );
    }
  }, [runList, runBaseline, runDiff]);

  // `?auto=1` runs the whole script once after first paint (used for unattended measurement).
  const autoStarted = useRef(false);
  const attach = useCallback(
    (node: HTMLDivElement | null) => {
      listRef.current = node;
      if (node && !autoStarted.current && new URLSearchParams(location.search).has("auto")) {
        autoStarted.current = true;
        setTimeout(() => void runAll(), 1500);
      }
    },
    [runAll],
  );

  return (
    <div className="flex h-full flex-col gap-2 p-3">
      <div className="flex items-center gap-3">
        <h1 className="m-0 text-[15px] font-semibold">WebView perf spike</h1>
        <button
          type="button"
          onClick={() => void runAll()}
          className="rounded-md border border-line bg-surface px-3 py-1.5 hover:bg-surface-2"
        >
          Run script
        </button>
        <span ref={meterRef} className="tnum font-mono text-[12px]" data-testid="meter">
          waiting
        </span>
        <span className="text-muted">{status}</span>
      </div>
      <div className="grid min-h-0 flex-1 grid-cols-2 gap-2">
        <VirtualList
          scrollRef={attach}
          count={rows.length}
          rowHeight={ROW_H}
          label="100k rows"
          className="rounded-md border border-line bg-surface"
          renderRow={(i) => {
            const row = rows[i];
            return row ? <FileRow path={row.path} status={row.status} lock={row.lock} /> : null;
          }}
        />
        <div ref={diffRef} className="min-h-0 overflow-hidden rounded-md border border-line" />
      </div>
      {result && (
        <pre className="m-0 max-h-40 overflow-auto rounded-md bg-surface-2 p-2 font-mono text-[11px]">
          {JSON.stringify(result, null, 2)}
        </pre>
      )}
    </div>
  );
}
