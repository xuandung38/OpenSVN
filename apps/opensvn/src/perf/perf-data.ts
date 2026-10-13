/** Deterministic synthetic data for the perf spike (no randomness so runs are comparable). */
import type { StatusCode } from "../lib/status";

const DIRS = ["src", "assets", "textures", "scripts", "docs", "build", "tools", "levels"];
const CODES: StatusCode[] = ["M", "A", "D", "R", "?", "!", "C"];

export interface PerfRow {
  path: string;
  status: StatusCode;
  lock?: "me" | "other";
}

export function makeRows(n: number): PerfRow[] {
  const rows: PerfRow[] = [];
  for (let i = 0; i < n; i++) {
    const d1 = DIRS[i % DIRS.length];
    const d2 = DIRS[(i * 7) % DIRS.length];
    const row: PerfRow = {
      path: `${d1}/${d2}/module-${i % 997}/file-${i}.${i % 3 === 0 ? "ts" : "txt"}`,
      status: CODES[i % CODES.length] ?? "M",
    };
    if (i % 41 === 0) row.lock = i % 2 === 0 ? "me" : "other";
    rows.push(row);
  }
  return rows;
}

/** Returns [original, modified] documents of `lines` lines where ~3% of lines differ. */
export function makeDiffDocs(lines: number): [string, string] {
  const a: string[] = [];
  const b: string[] = [];
  for (let i = 0; i < lines; i++) {
    const base = `export function fn${i}(x: number): number { return x * ${i % 97} + ${i}; }`;
    a.push(base);
    if (i % 33 === 0) b.push(`${base} // changed ${i}`);
    else if (i % 101 === 0) continue;
    else b.push(base);
    if (i % 250 === 0) b.push(`// inserted after ${i}`);
  }
  return [a.join("\n"), b.join("\n")];
}
