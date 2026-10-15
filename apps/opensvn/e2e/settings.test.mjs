// Settings opens from every entry point in the real app on WebKitGTK (the engine closest to
// macOS's WKWebView that runs on Linux). Needs no SVN server: a fresh profile starts on the Welcome
// screen, as a fresh install on a Mac does. Run with `pnpm e2e -- --test-name-pattern=Settings`
// or `node --test e2e/settings.test.mjs` (see e2e/README.md for the tools).

import assert from "node:assert/strict";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { afterEach, beforeEach, test } from "node:test";
import { App } from "./harness.mjs";
import { Keys } from "./webdriver.mjs";

const ARTIFACTS = process.env.OPENSVN_E2E_ARTIFACTS ?? join(process.cwd(), "e2e-artifacts");

/** @type {App} */
let app;

beforeEach(async () => {
  app = await App.start("settings");
  await app.waitText("Welcome to OpenSVN", 30_000);
});

afterEach(async (t) => {
  if (t.passed === false || process.exitCode) {
    mkdirSync(ARTIFACTS, { recursive: true });
    const base = join(ARTIFACTS, t.name.replace(/[^a-z0-9]+/gi, "-"));
    try {
      await app.screenshot(`${base}.png`);
      writeFileSync(`${base}.txt`, `${await app.bodyText()}\n\n--- driver log ---\n${app.log()}`);
    } catch {
      // the app may be gone
    }
  }
  await app.stop();
});

/**
 * The Settings sheet is open AND visible: a modal <dialog> in the top layer, laid out, painted
 * (opacity, visibility) and the topmost element at its own centre (not under another layer).
 */
async function waitSettingsVisible() {
  return app.waitFor(
    () =>
      app.exec(`
        const d = document.querySelector('dialog[aria-label="Settings"]');
        if (!d || !d.open || !d.matches(':modal')) return null;
        const r = d.getBoundingClientRect();
        const cs = getComputedStyle(d);
        if (r.width < 300 || r.height < 300 || cs.visibility !== 'visible' || cs.display === 'none' || Number(cs.opacity) < 1) return null;
        const top = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2);
        if (!top || !d.contains(top)) return null;
        return { w: r.width, h: r.height, display: cs.display, text: d.innerText.slice(0, 200) };
      `),
    15_000,
    "the Settings sheet to be open and visible",
  );
}

/** Replaces the value of the number input labelled `label` (the harness's `fill` covers text boxes). */
async function fillNumber(label, text) {
  const el = await app.waitFor(
    () =>
      app.exec(
        `return [...document.querySelectorAll('label')].find((l) => l.innerText.trim().startsWith(arguments[0]))?.querySelector('input[type=number]') ?? null`,
        label,
      ),
    10_000,
    `number input ${label}`,
  );
  const id = el["element-6066-11e4-a52e-4f735466cecf"];
  await app.s.clear(id);
  await app.s.type(id, text);
}

async function closeSettings() {
  await app.click("button", "Cancel");
  await app.waitFor(
    () => app.exec(`return !document.querySelector('dialog[aria-label="Settings"]')?.open`),
    10_000,
    "the Settings sheet to close",
  );
}

test("Settings: the toolbar gear opens the sheet on the Welcome screen, nothing covers the gear", async () => {
  const gear = await app.role("button", "Settings");
  // A pointer at the gear's centre hits the gear itself (no drag region or overlay on top).
  const hit = await app.exec(
    `const b = arguments[0]; const r = b.getBoundingClientRect();
     const top = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2);
     return { hit: !!top && b.contains(top), drag: !!top?.closest('[data-tauri-drag-region]') };`,
    gear,
  );
  assert.deepEqual(hit, { hit: true, drag: false });
  await app.click("button", "Settings");
  const sheet = await waitSettingsVisible();
  assert.match(sheet.text, /General/);
  assert.equal(sheet.display, "grid");
  await closeSettings();
});

test("Settings: Ctrl+, opens the sheet (Linux)", async () => {
  await app.keys(Keys.Control, ",");
  await waitSettingsVisible();
});

test("Settings: Cmd+, (metaKey) opens the sheet when the page reports macOS", async () => {
  // WKWebView reports navigator.platform "MacIntel"; the shortcut is then Meta, not Control.
  await app.exec(
    `Object.defineProperty(navigator, 'platform', { configurable: true, get: () => 'MacIntel' });`,
  );
  await app.keys(Keys.Control, ",");
  await new Promise((r) => setTimeout(r, 500));
  assert.equal(
    await app.exec(`return !!document.querySelector('dialog[aria-label="Settings"]')?.open`),
    false,
    "Ctrl+, is not the macOS shortcut",
  );
  await app.keys(Keys.Meta, ",");
  await waitSettingsVisible();
});

test("Settings: Tauri accepts the menu event names the UI subscribes to, and refuses dotted ones", async () => {
  // The same `plugin:event|listen` call the UI's event bus makes, through the real IPC.
  const result = await app.s.execAsync(
    `const done = arguments[arguments.length - 1];
     const ipc = window.__TAURI_INTERNALS__;
     const listen = (event) => ipc.invoke('plugin:event|listen', { event, target: { kind: 'Any' }, handler: ipc.transformCallback(() => {}) })
       .then(() => 'ok', (e) => 'refused: ' + String(e));
     Promise.all([listen('menu://app/settings'), listen('menu://app.settings')]).then(done);`,
  );
  assert.equal(result[0], "ok");
  assert.match(result[1], /^refused: .*Event name must include only/);
});

test("Settings: Network refuses 16 connections inline and saves 8 through the real backend", async () => {
  await app.click("button", "Settings");
  await waitSettingsVisible();
  await app.click("button", "Network");
  await fillNumber("Parallel connections (1–8)", "16");
  await app.waitText("Enter a whole number from 1 to 8");
  const save = await app.role("button", "Save");
  assert.equal(await app.exec("return arguments[0].disabled", save), true);
  await fillNumber("Parallel connections (1–8)", "8");
  await app.waitFor(
    () => app.exec("return !arguments[0].disabled", save),
    10_000,
    "Save to enable",
  );
  await app.click("button", "Save");
  await app.waitText("Settings saved");
  assert.doesNotMatch(await app.bodyText(), /Invalid value for setting/);
  await app.click("button", "Settings");
  await app.click("button", "Network");
  assert.equal(
    await app.exec(
      `return [...document.querySelectorAll('dialog[open] input[type=number]')].map((i) => i.value).join(',')`,
    ),
    "8,60,10",
  );
});
