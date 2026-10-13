// @vitest-environment node
import { expect, test } from "vitest";
// @ts-expect-error plain .mjs script without type declarations
import { scan } from "../scripts/lint-safety.mjs";

const labels = (src: string): string[] => (scan(src) as [number, string][]).map(([, l]) => l);

test.each([
  ["el.setHTMLUnsafe(x)", "setHTMLUnsafe"],
  ["range.createContextualFragment(s)", "createContextualFragment"],
  ['const f = Function("return 1")', "Function"],
  ["const f = new Function(a)", "Function"],
  ['setInterval("x()", 5)', "string timer"],
  ["<iframe\n src=x />", "iframe"],
  ['document.createElement("iframe")', "createElement"],
  ["x.innerHTML = s", "innerHTML"],
  ["frame.srcdoc = s", "srcdoc"],
  ["/* note */ el.innerHTML = s", "innerHTML"],
  ["run(); // el.outerHTML is bad\nel.outerHTML = s", "outerHTML"],
])("flags %s", (src, needle) => {
  expect(labels(src).join(",")).toContain(needle);
});

// Comment markers inside literals are code: a sink on a later line must still be found.
test.each([
  ['const g = "src/*";\nel.innerHTML = s;', "innerHTML"],
  ['<input accept="image/*" />\nel.innerHTML = s;', "innerHTML"],
  ['const p = "a//b";\nel.innerHTML = s;', "innerHTML"],
  ["const t = `x /* $" + "{y} // z`;\nel.innerHTML = s;", "innerHTML"],
  ["const t = `a $" + "{b + `c//d`} /*`;\nel.innerHTML = s;", "innerHTML"],
  ["const r = /a\\/*b/;\nel.innerHTML = s;", "innerHTML"],
  ["const r = /[/*]/;\nel.innerHTML = s;", "innerHTML"],
])("literal with comment markers does not hide later code: %s", (src, needle) => {
  expect(labels(src).join(",")).toContain(needle);
});

test("real comments are still ignored, including multi-line blocks", () => {
  expect(labels("/*\n el.innerHTML = s;\n*/\nconst ok = 1; // el.innerHTML")).toEqual([]);
});

test("does not flag comments or safe code", () => {
  expect(labels("// innerHTML is forbidden here")).toEqual([]);
  expect(labels("/* eval\n * Function(\n */\nconst a = 1;")).toEqual([]);
  expect(labels('const url = "https://x.test"; textContent = url;')).toEqual([]);
});
