#!/usr/bin/env node
// Forbids HTML-injection / dynamic-code sinks in app source (untrusted SVN content must render as text).
// Biome already covers `dangerouslySetInnerHTML` (noDangerouslySetInnerHtml), `eval` (noGlobalEval) and
// `javascript:` URLs; this scan adds the innerHTML family, Function constructors, iframe-like elements
// and string timers. CSP (`script-src 'self'`) is the runtime backstop for the eval-like ones.
import { readdirSync, readFileSync, statSync } from "node:fs";
import { extname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../src", import.meta.url));
// bindings.ts is generated; test files hold forbidden strings as XSS fixtures.
const SKIP = (p) => {
  const n = p.replaceAll("\\", "/");
  return n.endsWith("ipc/bindings.ts") || /\.test\.tsx?$/.test(n);
};

export const RULES = [
  [/dangerouslySetInnerHTML/, "dangerouslySetInnerHTML"],
  [/\b(innerHTML|outerHTML)\b/, "innerHTML/outerHTML"],
  [/\binsertAdjacentHTML\b/, "insertAdjacentHTML"],
  [/\b(setHTMLUnsafe|parseHTMLUnsafe)\b/, "setHTMLUnsafe/parseHTMLUnsafe"],
  [/\bcreateContextualFragment\b/, "createContextualFragment"],
  [/\bsrcdoc\b/, "srcdoc"],
  [/\bdocument\.write(ln)?\b/, "document.write"],
  [/(^|[^.\w])eval\b/, "eval"],
  [/\bFunction\s*\(/, "Function constructor/call"],
  [/\bset(Timeout|Interval)\s*\(\s*["'`]/, "string timer code"],
  [/<\s*(iframe|object|embed)\b/i, "iframe/object/embed element"],
  [/createElement\s*\(\s*["'`](iframe|object|embed)["'`]/i, "createElement(iframe/object/embed)"],
];

/** Removes comments but keeps code on the same line (so `x(); // note` is still scanned). */
export function stripComments(line, state) {
  let out = "";
  let i = 0;
  while (i < line.length) {
    if (state.inBlock) {
      const end = line.indexOf("*/", i);
      if (end === -1) return out;
      state.inBlock = false;
      i = end + 2;
    } else if (line.startsWith("/*", i)) {
      state.inBlock = true;
      i += 2;
    } else if (line.startsWith("//", i) && line[i - 1] !== ":") {
      return out;
    } else {
      out += line[i++];
    }
  }
  return out;
}

export function scan(text) {
  const hits = [];
  const state = { inBlock: false };
  text.split("\n").forEach((raw, i) => {
    const line = stripComments(raw, state);
    for (const [re, label] of RULES) if (re.test(line)) hits.push([i + 1, label]);
  });
  return hits;
}

function* walk(dir) {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) yield* walk(p);
    else if ([".ts", ".tsx", ".js", ".jsx"].includes(extname(p))) yield p;
  }
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const problems = [];
  for (const file of walk(root)) {
    if (SKIP(file)) continue;
    for (const [line, label] of scan(readFileSync(file, "utf8"))) {
      problems.push(`${relative(process.cwd(), file)}:${line}: ${label}`);
    }
  }
  if (problems.length > 0) {
    console.error(`lint:safety failed (${problems.length}):\n${problems.join("\n")}`);
    process.exit(1);
  }
  console.log("lint:safety ok");
}
