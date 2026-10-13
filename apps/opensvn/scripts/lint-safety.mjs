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

/**
 * Returns `text` with comments blanked out (newlines kept, so line numbers stay valid). Quote-aware:
 * `/*` or `//` inside string, template or regex literals is code, not a comment. String contents are
 * kept on purpose because some rules match string arguments (e.g. createElement("iframe")).
 */
export function stripComments(text) {
  let out = "";
  let i = 0;
  let prev = ""; // last significant (non-space) code character, to tell regex from division
  const templateDepths = []; // brace depth at which each open `${` expression returns to template
  let braces = 0;
  const n = text.length;

  const readQuoted = (quote) => {
    out += text[i++];
    while (i < n && text[i] !== quote) {
      if (text[i] === "\\" && i + 1 < n) out += text[i++];
      if (quote !== "`" && text[i] === "\n") break; // unterminated: do not swallow the file
      if (quote === "`" && text[i] === "$" && text[i + 1] === "{") {
        out += "${";
        i += 2;
        templateDepths.push(braces);
        braces++;
        return "expr";
      }
      out += text[i++];
    }
    if (i < n && text[i] === quote) out += text[i++];
    prev = quote;
    return "done";
  };
  const resumeTemplate = () => {
    // Called right after the `}` that closes a `${...}`: continue reading the template string.
    while (i < n && text[i] !== "`") {
      if (text[i] === "\\" && i + 1 < n) out += text[i++];
      if (text[i] === "$" && text[i + 1] === "{") {
        out += "${";
        i += 2;
        templateDepths.push(braces);
        braces++;
        return;
      }
      out += text[i++];
    }
    if (i < n) out += text[i++];
    prev = "`";
  };

  while (i < n) {
    const c = text[i];
    const d = text[i + 1];
    if (c === "/" && d === "/") {
      while (i < n && text[i] !== "\n") i++;
    } else if (c === "/" && d === "*") {
      i += 2;
      while (i < n && !(text[i] === "*" && text[i + 1] === "/")) {
        if (text[i] === "\n") out += "\n";
        i++;
      }
      i += 2;
      out += " ";
    } else if (c === '"' || c === "'" || c === "`") {
      readQuoted(c);
    } else if (c === "/" && (prev === "" || "(,=:[!&|?{};+-*%<>~^".includes(prev))) {
      // Regex literal: copy through the closing slash (slashes inside [...] do not close it).
      out += text[i++];
      let inClass = false;
      while (i < n && text[i] !== "\n" && (inClass || text[i] !== "/")) {
        if (text[i] === "\\" && i + 1 < n) out += text[i++];
        else if (text[i] === "[") inClass = true;
        else if (text[i] === "]") inClass = false;
        out += text[i++];
      }
      if (i < n && text[i] === "/") out += text[i++];
      prev = "/";
    } else {
      if (c === "{") braces++;
      if (c === "}") {
        braces--;
        if (templateDepths.length > 0 && templateDepths[templateDepths.length - 1] === braces) {
          templateDepths.pop();
          out += text[i++];
          resumeTemplate();
          continue;
        }
      }
      if (!/\s/.test(c)) prev = c;
      out += text[i++];
    }
  }
  return out;
}

export function scan(text) {
  const hits = [];
  stripComments(text)
    .split("\n")
    .forEach((line, i) => {
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
