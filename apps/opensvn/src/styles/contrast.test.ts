// @vitest-environment node
import { readFileSync } from "node:fs";
import { expect, test } from "vitest";

const css = readFileSync(new URL("./tokens.css", import.meta.url), "utf8");

/** Token map for one theme: the light block (`:root`) or the `[data-theme="dark"]` block. */
function tokens(theme: "light" | "dark"): Record<string, string> {
  const start = theme === "light" ? css.indexOf(":root,") : css.indexOf('[data-theme="dark"] {');
  const body = css.slice(css.indexOf("{", start) + 1, css.indexOf("}", start));
  return Object.fromEntries(
    [...body.matchAll(/--([\w-]+):\s*(#[0-9a-fA-F]{6})\s*;/g)].map((m) => [m[1], m[2]]),
  ) as Record<string, string>;
}

function lum(hex: string): number {
  const c = [1, 3, 5].map((i) => Number.parseInt(hex.slice(i, i + 2), 16) / 255);
  const [r, g, b] = c.map((x) => (x <= 0.03928 ? x / 12.92 : ((x + 0.055) / 1.055) ** 2.4)) as [
    number,
    number,
    number,
  ];
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}
function ratio(a: string, b: string): number {
  const [hi, lo] = [lum(a), lum(b)].sort((x, y) => y - x) as [number, number];
  return (hi + 0.05) / (lo + 0.05);
}

const TEXT_ON: Record<string, string[]> = {
  "muted-text": ["surface", "surface-2", "sel", "bg", "accent-soft"],
  "accent-text": ["surface", "surface-2", "sel", "accent-soft"],
  "ok-text": ["surface", "surface-2", "sel", "add-bg"],
  "warn-text": ["surface", "surface-2", "sel", "warn-soft"],
  "bad-text": ["surface", "surface-2", "sel", "bad-soft", "del-bg"],
  "info-text": ["surface", "surface-2", "sel"],
  fg: ["surface", "surface-2", "sel", "warn-soft", "bad-soft", "accent-soft"],
};
const FILLS: [string, string][] = [
  ["on-accent", "accent"],
  ["on-bad", "bad"],
  ["on-warn", "warn"],
];

for (const theme of ["light", "dark"] as const) {
  test(`${theme}: text tokens reach WCAG AA (4.5:1) on the surfaces they are used on`, () => {
    const t = tokens(theme);
    for (const [fg, bgs] of Object.entries(TEXT_ON)) {
      for (const bg of bgs) {
        expect(t[fg] && t[bg], `${fg}/${bg} defined`).toBeTruthy();
        expect(
          ratio(t[fg] as string, t[bg] as string),
          `${theme} ${fg} on ${bg}`,
        ).toBeGreaterThanOrEqual(4.5);
      }
    }
    for (const [fg, bg] of FILLS) {
      expect(
        ratio(t[fg] as string, t[bg] as string),
        `${theme} ${fg} on ${bg}`,
      ).toBeGreaterThanOrEqual(4.5);
    }
  });
}
