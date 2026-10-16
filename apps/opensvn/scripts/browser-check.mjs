#!/usr/bin/env node
// `pnpm test:browser` (optional gate, see AGENTS.md): starts the mock UI (`vite --mode mock`) and,
// in Chromium and WebKit, in the light and the dark theme,
//   - runs axe-core (WCAG 2.2 A/AA rules) on the main window, the tree view with ignored files, the image onion skin and
//     the shortcuts dialog, including `color-contrast` (jsdom cannot compute it);
//   - checks that the image sliders move with the arrow keys (jsdom does not step range inputs);
//   - runs axe-core on Settings › Updates and on the app update dialog (`?update=available`);
//   - runs axe-core on Branch › Compare with branch (`?menu=branch/compare`): a bad revision, the
//     summary at chosen revisions, a file diff and the working copy with local changes;
//   - runs axe-core on the gallery, which shows the "connection lost" notice;
//   - takes screenshots of those states and of `/__gallery` into `test-results/browser/`.
// Both browsers run by default; one that is not installed is skipped with a note (Playwright's
// WebKit needs `pnpm exec playwright-core install webkit`, and on Linux its system libraries:
// `pnpm exec playwright-core install-deps webkit`). The run fails when no browser ran or axe finds
// a violation.
//
// Options: `--browsers=chromium,webkit` (default both), `--keep-server` to leave Vite running.
import { spawn } from "node:child_process";
import { mkdirSync, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { createServer } from "node:net";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const appDir = join(dirname(fileURLToPath(import.meta.url)), "..");
const outDir = join(appDir, "test-results", "browser");
const axeSource = readFileSync(require.resolve("axe-core/axe.min.js"), "utf8");
const args = process.argv.slice(2).filter((a) => a !== "--");
const wanted = (args.find((a) => a.startsWith("--browsers="))?.split("=")[1] ?? "chromium,webkit")
  .split(",")
  .map((b) => b.trim())
  .filter(Boolean);

function freePort() {
  return new Promise((resolve, reject) => {
    const srv = createServer();
    srv.once("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

async function startVite(port) {
  const vite = join(dirname(require.resolve("vite/package.json")), "bin", "vite.js");
  const child = spawn(
    process.execPath,
    [vite, "--mode", "mock", "--port", String(port), "--strictPort", "--host", "127.0.0.1"],
    { cwd: appDir, stdio: ["ignore", "pipe", "pipe"] },
  );
  let log = "";
  child.stdout.on("data", (d) => {
    log += d;
  });
  child.stderr.on("data", (d) => {
    log += d;
  });
  const url = `http://127.0.0.1:${port}`;
  for (let i = 0; i < 150; i++) {
    if (child.exitCode !== null) throw new Error(`vite exited early:\n${log}`);
    try {
      const r = await fetch(url);
      if (r.ok) return { child, url };
    } catch {
      // not up yet
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  child.kill();
  throw new Error(`vite did not start:\n${log}`);
}

async function axe(page, label) {
  if (!(await page.evaluate(() => "axe" in window)))
    await page.addScriptTag({ content: axeSource });
  const violations = await page.evaluate(async () => {
    // WCAG 2.2 A/AA rules (color-contrast included); best-practice rules such as "one h1 per
    // page" do not fit a desktop app window.
    const r = await window.axe.run(document, {
      resultTypes: ["violations"],
      runOnly: { type: "tag", values: ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "wcag22aa"] },
    });
    return r.violations.map((v) => ({
      id: v.id,
      help: v.help,
      nodes: v.nodes.slice(0, 5).map((n) => `${n.target.join(" ")}: ${n.failureSummary ?? ""}`),
    }));
  });
  for (const v of violations) {
    console.error(`  ✗ ${label}: ${v.id}: ${v.help}`);
    for (const n of v.nodes) console.error(`      ${n.replace(/\s+/g, " ")}`);
  }
  return violations.length;
}

/** UI labels the script looks for, in the language the mock UI shows (`<html lang>`). */
const LABELS = {
  en: {
    imageMode: "Image comparison",
    onion: "Onion skin",
    swipe: "Swipe",
    opacity: "Opacity of the new image",
    swipePos: "Swipe position",
    shortcuts: "Keyboard shortcuts",
    settings: "Settings",
    updates: "Updates",
    checkNow: "Check for updates",
    updateDialog: "App updates",
    install: "Install",
    compareDialog: "Compare with branch",
    branch: "Branch or tag",
    rightRev: "Revision of the other folder",
    rightNumber: "Revision of the other folder: revision number",
    compare: "Compare",
    local: "Include local changes (compare the working copy itself)",
    table: /^Differences between/,
    showDiff: "Show diff of src/engine/renderer.ts",
  },
  vi: {
    imageMode: "Cách so sánh ảnh",
    onion: "Chồng lớp",
    swipe: "Trượt",
    opacity: "Độ đậm của ảnh mới",
    swipePos: "Vị trí thanh trượt",
    shortcuts: "Phím tắt",
    settings: "Cài đặt",
    updates: "Cập nhật",
    checkNow: "Kiểm tra cập nhật",
    updateDialog: "Cập nhật ứng dụng",
    install: "Cài bản cập nhật",
    compareDialog: "So sánh với branch",
    branch: "Branch hoặc tag",
    rightRev: "Revision của thư mục bên kia",
    rightNumber: "Revision của thư mục bên kia: số revision",
    compare: "So sánh",
    local: "Tính cả thay đổi local (so sánh chính working copy)",
    table: /^Khác biệt giữa/,
    showDiff: "Xem diff của src/engine/renderer.ts",
  },
};

async function shot(page, name) {
  await page.screenshot({ path: join(outDir, `${name}.png`), fullPage: true });
}

/** One browser and theme: every state, axe on each, screenshots. Returns the violation count. */
async function checkTheme(browser, browserName, theme, url) {
  const context = await browser.newContext({
    colorScheme: theme,
    viewport: { width: 1280, height: 800 },
  });
  const page = await context.newPage();
  const tag = `${browserName}-${theme}`;
  let failures = 0;
  const pageErrors = [];
  page.on("pageerror", (e) => pageErrors.push(String(e)));

  // Main window (mock data), Changes list as a tree with ignored files shown.
  await page.addInitScript(() => {
    localStorage.setItem("opensvn.viewPrefs", JSON.stringify({ tree: true, showIgnored: true }));
  });
  await page.goto(`${url}/`);
  await page
    .getByRole("option", { name: /hero-diffuse\.png/ })
    .first()
    .waitFor();
  await page
    .getByRole("option", { name: /game\.exe/ })
    .first()
    .waitFor();
  const lang = (await page.evaluate(() => document.documentElement.lang)) === "vi" ? "vi" : "en";
  const L = LABELS[lang];
  failures += await axe(page, `${tag} main window (tree, ignored)`);
  await shot(page, `${tag}-main-tree`);

  // Image diff: onion skin, and the slider moves with the keyboard.
  await page
    .getByRole("option", { name: /hero-diffuse\.png/ })
    .first()
    .click();
  const modes = page.getByRole("group", { name: L.imageMode });
  await modes.getByRole("button", { name: L.onion }).click();
  const slider = page.getByRole("slider", { name: L.opacity });
  await slider.focus();
  await page.keyboard.press("ArrowRight");
  const opacity = await page.locator("img[data-overlay]").evaluate((el) => el.style.opacity);
  if (opacity !== "0.55") {
    console.error(`  ✗ ${tag}: ArrowRight on the opacity slider gave opacity ${opacity}, not 0.55`);
    failures++;
  }
  failures += await axe(page, `${tag} image onion skin`);
  await shot(page, `${tag}-image-onion`);
  await modes.getByRole("button", { name: L.swipe }).click();
  await page.getByRole("slider", { name: L.swipePos }).focus();
  await page.keyboard.press("Home");
  const clip = await page.locator("img[data-overlay]").evaluate((el) => el.style.clipPath);
  if (!/100%/.test(clip)) {
    console.error(`  ✗ ${tag}: Home on the swipe slider gave clip-path ${clip}`);
    failures++;
  }
  await shot(page, `${tag}-image-swipe`);

  // Shortcuts dialog (Mod+/; the platform is pinned with ?os=win so Ctrl works everywhere).
  await page.goto(`${url}/?os=win`);
  await page
    .getByRole("option", { name: /hero-diffuse\.png/ })
    .first()
    .waitFor();
  await page.keyboard.press("Control+/");
  await page.getByRole("dialog", { name: L.shortcuts }).waitFor();
  failures += await axe(page, `${tag} shortcuts dialog`);
  await shot(page, `${tag}-shortcuts`);

  // Settings › Updates and the update dialog with a new version (`?update=available`).
  await page.goto(`${url}/?os=win&update=available`);
  await page
    .getByRole("option", { name: /hero-diffuse\.png/ })
    .first()
    .waitFor();
  await page.keyboard.press("Control+,");
  const sheet = page.getByRole("dialog", { name: L.settings });
  await sheet.waitFor();
  await sheet.getByRole("navigation").getByRole("button", { name: L.updates }).click();
  await sheet.getByText("0.1.0").waitFor();
  failures += await axe(page, `${tag} settings updates`);
  await shot(page, `${tag}-settings-updates`);
  await sheet.getByRole("button", { name: L.checkNow }).click();
  const update = page.getByRole("dialog", { name: L.updateDialog });
  await update.getByRole("button", { name: L.install }).waitFor();
  failures += await axe(page, `${tag} update available`);
  await shot(page, `${tag}-update-available`);

  // Branch › Compare with branch, opened like the native menu item would.
  await page.goto(`${url}/?os=win&menu=branch/compare`);
  const cmp = page.getByRole("dialog", { name: L.compareDialog });
  await cmp.waitFor();
  await cmp.getByRole("combobox", { name: L.branch }).selectOption({ label: "feature-x" });
  await cmp.getByRole("combobox", { name: L.rightRev }).selectOption("rev");
  const number = cmp.getByRole("textbox", { name: L.rightNumber });
  await number.fill("x1");
  await cmp.getByRole("alert").waitFor();
  failures += await axe(page, `${tag} compare, bad revision`);
  await shot(page, `${tag}-compare-bad-revision`);
  await number.fill("48100");
  await cmp.getByRole("button", { name: L.compare, exact: true }).click();
  await cmp.getByRole("table", { name: L.table }).waitFor();
  failures += await axe(page, `${tag} compare summary`);
  await shot(page, `${tag}-compare-summary`);
  await cmp.getByRole("button", { name: L.showDiff }).click();
  await cmp.locator(".cm-editor, .cm-mergeView").first().waitFor();
  failures += await axe(page, `${tag} compare file diff`);
  await shot(page, `${tag}-compare-diff`);
  await cmp.getByRole("checkbox", { name: L.local }).check();
  await cmp.getByRole("button", { name: L.compare, exact: true }).click();
  await cmp.getByRole("table", { name: L.table }).waitFor();
  failures += await axe(page, `${tag} compare with local changes`);
  await shot(page, `${tag}-compare-local`);

  // Component gallery (the "connection lost" notice included).
  await page.goto(`${url}/__gallery`);
  await page.waitForLoadState("networkidle");
  await page.getByTestId("connection-wait").first().waitFor();
  failures += await axe(page, `${tag} gallery`);
  await shot(page, `${tag}-gallery`);

  for (const e of pageErrors) {
    console.error(`  ✗ ${tag}: page error: ${e}`);
    failures++;
  }
  await context.close();
  return failures;
}

async function main() {
  mkdirSync(outDir, { recursive: true });
  const pw = await import("playwright-core");
  const { child, url } = await startVite(await freePort());
  let failures = 0;
  let ran = 0;
  try {
    for (const name of wanted) {
      const type = pw[name];
      if (!type) throw new Error(`unknown browser ${name}`);
      let browser;
      try {
        browser = await type.launch();
      } catch (e) {
        console.warn(`- ${name}: skipped (${String(e).split("\n")[0]})`);
        continue;
      }
      for (const theme of ["light", "dark"]) {
        const n = await checkTheme(browser, name, theme, url);
        console.log(`${n === 0 ? "✓" : "✗"} ${name} ${theme}: ${n} problem(s)`);
        failures += n;
      }
      await browser.close();
      ran++;
    }
  } finally {
    if (!args.includes("--keep-server")) child.kill();
  }
  console.log(`screenshots: ${outDir}`);
  if (ran === 0) {
    console.error("no browser could be launched");
    process.exit(1);
  }
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
