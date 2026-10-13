#!/usr/bin/env node
// Release-bundle guard: mock IPC data and dev-only routes must never ship.
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const dist = fileURLToPath(new URL("../dist", import.meta.url));
const FORBIDDEN = ["mock-op-", "is not implemented", "__perf-report", "/__gallery", "/__perf"];

function* walk(dir) {
  for (const n of readdirSync(dir)) {
    const p = join(dir, n);
    if (statSync(p).isDirectory()) yield* walk(p);
    else if (p.endsWith(".js") || p.endsWith(".html")) yield p;
  }
}

const bad = [];
for (const f of walk(dist)) {
  const text = readFileSync(f, "utf8");
  for (const needle of FORBIDDEN) if (text.includes(needle)) bad.push(`${f}: contains "${needle}"`);
}
if (bad.length > 0) {
  console.error(`check-dist failed:\n${bad.join("\n")}`);
  process.exit(1);
}
console.log("check-dist ok");
