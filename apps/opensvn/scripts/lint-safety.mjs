#!/usr/bin/env node
// Forbids HTML-injection / dynamic-code sinks in app source (untrusted SVN content must render as text).
// Biome already covers `dangerouslySetInnerHTML` (noDangerouslySetInnerHtml) and `eval` (noGlobalEval);
// this check adds innerHTML-family sinks, Function constructor, document.write and iframe/object/embed.
import { readdirSync, readFileSync, statSync } from "node:fs";
import { extname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../src", import.meta.url));
// bindings.ts is generated; test files may contain the forbidden strings as XSS fixtures.
const SKIP = (p) => p.endsWith("ipc/bindings.ts") || /\.test\.tsx?$/.test(p);

const RULES = [
  [/dangerouslySetInnerHTML/, "dangerouslySetInnerHTML"],
  [/\.(innerHTML|outerHTML)\b/, "innerHTML/outerHTML assignment"],
  [/\binsertAdjacentHTML\b/, "insertAdjacentHTML"],
  [/\bdocument\.write(ln)?\s*\(/, "document.write"],
  [/(^|[^.\w])eval\s*\(/, "eval()"],
  [/\bnew\s+Function\s*\(/, "new Function()"],
  [/\bsetTimeout\s*\(\s*["'`]/, "setTimeout with string code"],
  [/<(iframe|object|embed)[\s>]/i, "iframe/object/embed element"],
];

function* walk(dir) {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) yield* walk(p);
    else if ([".ts", ".tsx", ".js", ".jsx"].includes(extname(p))) yield p;
  }
}

const problems = [];
for (const file of walk(root)) {
  if (SKIP(file)) continue;
  readFileSync(file, "utf8")
    .split("\n")
    .forEach((line, i) => {
      // Ignore pure comment lines so documentation can name the forbidden APIs.
      if (/^\s*(\/\/|\*|\/\*)/.test(line)) return;
      for (const [re, label] of RULES) {
        if (re.test(line)) problems.push(`${relative(process.cwd(), file)}:${i + 1}: ${label}`);
      }
    });
}

if (problems.length > 0) {
  console.error(`lint:safety failed (${problems.length}):\n${problems.join("\n")}`);
  process.exit(1);
}
console.log("lint:safety ok");
