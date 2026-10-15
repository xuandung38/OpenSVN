import axe from "axe-core";

/**
 * axe-core rules turned off because jsdom cannot evaluate them, not because the app passes them:
 * - color-contrast: jsdom has no layout or computed colours (CSS is not loaded in tests, and it does
 *   not resolve custom properties), so every text node would come back "incomplete". Contrast of the
 *   design tokens is checked separately by src/styles/contrast.test.ts.
 * - link-in-text-block: same limitation, it compares the computed colour of a link with its
 *   surrounding text.
 */
const JSDOM_UNSUPPORTED_RULES = ["color-contrast", "link-in-text-block"] as const;

/** Mirrors index.html (title, <html lang>), which jsdom's blank document does not have. */
export function prepareDocument(lang: string): void {
  document.title = "OpenSVN";
  document.documentElement.lang = lang;
}

/** Impacts that fail a check; "moderate" and "minor" findings are only logged. */
const BLOCKING = new Set(["serious", "critical"]);

export interface AxeFinding {
  id: string;
  impact: string;
  help: string;
  targets: string[];
}

function finding(v: axe.Result): AxeFinding {
  return {
    id: v.id,
    impact: v.impact ?? "unknown",
    help: v.help,
    targets: v.nodes.map((n) => n.target.map(String).join(" ")),
  };
}

export function formatFindings(list: readonly AxeFinding[]): string {
  return list
    .map(
      (f) => `- ${f.id} [${f.impact}] ${f.help}\n${f.targets.map((t) => `    at ${t}`).join("\n")}`,
    )
    .join("\n");
}

/**
 * jsdom has no layout, so `document.elementFromPoint` is missing. axe uses it (through its
 * `elementsFromPoint` polyfill) to find the modal <dialog> on top; without it every rule that asks
 * "is this node inert behind a modal?" throws and axe silently files the rule under "incomplete".
 * Reporting "nothing at this point" makes axe treat no dialog as modal, so the whole document is
 * scanned: stricter than a browser, never weaker.
 */
function withElementFromPoint<T>(run: () => Promise<T>): Promise<T> {
  const doc = document as { elementFromPoint?: unknown };
  if (typeof doc.elementFromPoint === "function") return run();
  doc.elementFromPoint = () => null;
  return run().finally(() => {
    delete doc.elementFromPoint;
  });
}

/**
 * Runs axe over the whole document (Radix popovers and menus portal to <body>) and throws when a
 * serious or critical violation is found, naming rule, impact and target nodes. Lesser findings are
 * logged with `label` so they stay visible without failing the run. A rule that crashed (axe reports
 * it as "incomplete" with an error) also fails: a jsdom gap must not pass for a clean result.
 */
export async function expectNoSeriousA11yViolations(label: string, root: Element = document.body) {
  const results = await withElementFromPoint(() =>
    axe.run(root, {
      resultTypes: ["violations", "incomplete"],
      rules: Object.fromEntries(JSDOM_UNSUPPORTED_RULES.map((id) => [id, { enabled: false }])),
    }),
  );
  const crashed = results.incomplete
    .map((r) => ({ id: r.id, error: (r as { error?: { message?: string } }).error }))
    .filter((r) => r.error);
  if (crashed.length > 0) {
    throw new Error(
      `[axe] ${label}: rules failed to run\n${crashed.map((r) => `- ${r.id}: ${r.error?.message}`).join("\n")}`,
    );
  }
  const all = results.violations.map(finding);
  const blocking = all.filter((f) => BLOCKING.has(f.impact));
  const minor = all.filter((f) => !BLOCKING.has(f.impact));
  if (minor.length > 0) {
    console.info(`[axe] ${label}: non-blocking findings\n${formatFindings(minor)}`);
  }
  if (blocking.length > 0) {
    throw new Error(`[axe] ${label}: serious/critical violations\n${formatFindings(blocking)}`);
  }
}
