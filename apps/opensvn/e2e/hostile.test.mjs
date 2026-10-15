// Hostile repository (#18): the real app and the real engine check out the repository of
// fixtures/evil/make-hostile-repo.sh from the evil server (docker/evil-server/run-local.sh, Apache
// + mod_dav_svn over HTTPS, no Docker), render its log, its file list and its properties, and
// nothing in it executes: no dialog, no injected DOM, no network request. Versioned symlinks that
// leave the working copy are refused by the open/reveal commands (PathGrant semantics: the
// WebView never gets a path outside a registered working copy acted on, and a forged grant is
// refused), and no launcher runs.
//
// Needs what flows.test.mjs needs (tauri-driver, WebKitWebDriver, xvfb, the debug build with the
// real engine) plus Apache with mod_dav_svn and svn/svnadmin/svnmucc on PATH. Run:
//   OPENSVN_E2E_SKIP_BUILD=1 xvfb-run -a node --test-timeout=300000 e2e/hostile.test.mjs

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import {
  chmodSync,
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  readlinkSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { createServer as createHttpServer } from "node:http";
import { createServer as createNetServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { after, afterEach, before, test } from "node:test";
import { fileURLToPath } from "node:url";
import { App, PASSWORD, USER } from "./harness.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "../../..");
const ARTIFACTS = process.env.OPENSVN_E2E_ARTIFACTS ?? join(process.cwd(), "e2e-artifacts");

const IMG_NAME = "<img src=x onerror=alert('name')>.txt";
const SCRIPT_NAME = "<script>alert('name')<%2Fscript>.txt";
const RLO_NAME = "invoice\u202Etxt.exe";
const TEMPLATE_NAME = "{{constructor.constructor('alert(1)')()}}.txt";
const ENTITY_NAME = "&lt;img src=x onerror=alert('entity')&gt;.txt";
const PERCENT_NAME = "%3Cscript%3Ealert('percent')%3C%2Fscript%3E.txt";

/** Every request that reaches the beacon listener (any host the repository points at). */
const beaconHits = [];
let beacon;
let evilDir;
let evilUrl;
let fakeBin;
let launchLog;
/** @type {App} */
let app;

async function freePort() {
  return new Promise((ok, fail) => {
    const s = createNetServer();
    s.unref();
    s.on("error", fail);
    s.listen(0, "127.0.0.1", () => {
      const { port } = s.address();
      s.close(() => ok(port));
    });
  });
}

before(async () => {
  beacon = createHttpServer((req, res) => {
    beaconHits.push(`${req.method} ${req.url}`);
    res.end();
  });
  await new Promise((ok) => beacon.listen(0, "127.0.0.1", ok));
  const beaconUrl = `http://127.0.0.1:${beacon.address().port}`;

  evilDir = mkdtempSync(join(tmpdir(), "opensvn-e2e-evil-"));
  const port = await freePort();
  execFileSync("bash", [join(repoRoot, "docker/evil-server/run-local.sh"), "up"], {
    env: { ...process.env, EVIL_DIR: evilDir, EVIL_PORT: String(port), BEACON: beaconUrl },
    stdio: ["ignore", "ignore", "inherit"],
  });
  evilUrl = `https://localhost:${port}/svn/hostile`;

  // Stand-ins for every launcher the app could reach on Linux: they only record their arguments.
  fakeBin = mkdtempSync(join(tmpdir(), "opensvn-e2e-launchers-"));
  launchLog = join(fakeBin, "launched.log");
  for (const tool of ["xdg-open", "gio", "gvfs-open", "nautilus", "dolphin", "nemo", "thunar"]) {
    const p = join(fakeBin, tool);
    writeFileSync(p, `#!/bin/sh\nprintf '%s %s\\n' "$0" "$*" >> '${launchLog}'\n`);
    chmodSync(p, 0o755);
  }
});

after(async () => {
  try {
    execFileSync("bash", [join(repoRoot, "docker/evil-server/run-local.sh"), "purge"], {
      env: { ...process.env, EVIL_DIR: evilDir },
      stdio: "ignore",
    });
  } catch {
    // best effort
  }
  beacon?.close();
  if (fakeBin) rmSync(fakeBin, { recursive: true, force: true });
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
  await app?.stop();
});

/**
 * Installed in the page before any hostile content is shown: records dialogs, `window.open`,
 * nodes that could run code or load something, and every resource the page fetches. A navigation
 * (a meta refresh, a `javascript:` link) would drop the recorder itself, which the final check
 * notices.
 */
const INSTALL_TRAPS = `
  const w = window;
  w.__e2eTraps = { dialogs: [], nodes: [], resources: [] };
  for (const name of ["alert", "confirm", "prompt", "print", "open"]) {
    w[name] = (...args) => { w.__e2eTraps.dialogs.push(name + ":" + args.map(String).join(",")); return null; };
  }
  const risky = (el) => {
    if (!(el instanceof Element)) return null;
    const tag = el.tagName.toLowerCase();
    if (["script", "iframe", "object", "embed", "frame", "base", "meta", "link", "foreignobject"].includes(tag)) return tag;
    for (const a of el.attributes) {
      if (/^on/i.test(a.name)) return tag + "[" + a.name + "]";
      if (/^\\s*(javascript|vbscript|data):/i.test(a.value) && /^(href|src|action|formaction|xlink:href)$/i.test(a.name)) return tag + "[" + a.name + "=" + a.value + "]";
    }
    if (tag === "img" || tag === "image" || tag === "source" || tag === "video" || tag === "audio") {
      const src = el.getAttribute("src") || el.getAttribute("href") || "";
      if (!/^(data:image\\/|blob:|asset:|https?:\\/\\/asset\\.localhost|\\/)/.test(src) && src !== "") return tag + "[src=" + src + "]";
    }
    return null;
  };
  new MutationObserver((records) => {
    for (const r of records) {
      const nodes = r.type === "attributes" ? [r.target] : [...r.addedNodes];
      for (const n of nodes) {
        const hit = risky(n);
        if (hit) w.__e2eTraps.nodes.push(hit);
        if (n instanceof Element) for (const c of n.querySelectorAll("*")) { const h = risky(c); if (h) w.__e2eTraps.nodes.push(h); }
      }
    }
  }).observe(document.documentElement, { childList: true, subtree: true, attributes: true });
  new PerformanceObserver((list) => {
    for (const e of list.getEntries()) w.__e2eTraps.resources.push(e.name);
  }).observe({ type: "resource", buffered: false });
  return true;
`;

async function traps() {
  return app.exec("return window.__e2eTraps ?? null");
}

/** Nothing ran, nothing was injected, nothing was fetched from outside the app. */
async function assertNothingExecuted(stage) {
  const t = await traps();
  assert.ok(t, `${stage}: the page navigated away (the trap recorder is gone)`);
  assert.deepEqual(t.dialogs, [], `${stage}: dialogs or window.open were called`);
  assert.deepEqual(t.nodes, [], `${stage}: risky nodes were inserted`);
  const outside = t.resources.filter(
    (u) => !/^(ipc|tauri|asset):|^https?:\/\/(ipc|tauri|asset)\.localhost\//.test(u),
  );
  assert.deepEqual(outside, [], `${stage}: the page fetched resources`);
  const pwned = await app.exec("return window.__opensvnPwned ?? null");
  assert.equal(pwned, null, `${stage}: a hostile script set its marker`);
  // No JavaScript dialog is open (WebDriver reports "no such alert").
  await assert.rejects(app.s.cmd("GET", "/alert/text"), /no such alert/, `${stage}: dialog open`);
  assert.deepEqual(beaconHits, [], `${stage}: the beacon listener got requests`);
  assert.equal(existsSync(launchLog), false, `${stage}: a launcher ran`);
}

/** Clicks the last visible element with `role` whose text matches `name` (the main area comes
 * after the sidebar, which has a tab and rows with the same names). */
async function clickLast(role, name, minCount = 1, timeout = 30_000) {
  const el = await app.waitFor(
    () =>
      app.exec(
        `const [role, src, flags] = arguments; const re = new RegExp(src, flags);
         const els = [...document.querySelectorAll('[role="' + role + '"]' + (role === 'button' ? ',button' : ''))].filter((e) => {
           const r = e.getBoundingClientRect();
           return r.width > 0 && r.height > 0 && re.test((e.getAttribute('aria-label') || e.innerText || '').trim());
         });
         return els.length >= arguments[3] ? els[els.length - 1] : null;`,
        role,
        name.source,
        name.flags,
        minCount,
      ),
    timeout,
    `${role} ${name}`,
  );
  await app.s.click(el["element-6066-11e4-a52e-4f735466cecf"]);
}

/** WebView-side `invoke` of a backend command, as any script in the page could do. */
async function invoke(cmd, args) {
  return app.s.execAsync(
    `const [cmd, args, done] = arguments;
     window.__TAURI_INTERNALS__.invoke(cmd, args).then(
       (v) => done({ ok: true, value: v ?? null }),
       (e) => done({ ok: false, error: typeof e === "string" ? e : JSON.stringify(e) }),
     );`,
    cmd,
    args,
  );
}

test("hostile repository renders as text, runs nothing, and its escaping symlinks are refused", async () => {
  app = await App.start("hostile", { PATH: `${fakeBin}:${process.env.PATH}` });
  assert.equal(await app.exec(INSTALL_TRAPS), true);
  // Control: the recorder notices an injected handler and a dialog call, then starts clean.
  await app.exec(
    `const d = document.createElement("div"); d.hidden = true;
     d.innerHTML = '<b onclick="x()">control</b>'; document.body.append(d); window.alert("control");`,
  );
  await app.waitFor(async () => (await traps()).nodes.length > 0, 5_000, "trap control");
  assert.deepEqual((await traps()).dialogs, ["alert:control"]);
  await app.exec(
    `document.querySelector('[onclick="x()"]').parentElement.remove();
     window.__e2eTraps.nodes = []; window.__e2eTraps.dialogs = [];`,
  );

  // Checkout trunk through the real engine (trust the throwaway CA, Basic sign-in).
  await app.click("button", "Checkout repository…");
  await app.fill("Repository URL", `${evilUrl}/trunk`);
  await app.click("button", "Checkout");
  await app.click("button", "Trust permanently", 30_000);
  await app.role("textbox", "Username", 30_000);
  await app.fill("Username", USER);
  await app.fill("Password", PASSWORD);
  await app.click("button", "Sign in");
  await app.waitText("Checked out hostile", 90_000);
  await app.waitText("No local changes");
  const [wcName] = readdirSync(join(app.home, "svn"));
  const wc = join(app.home, "svn", wcName);
  assert.equal(readlinkSync(join(wc, "escape-abs")), "/etc/passwd");
  assert.ok(lstatSync(join(wc, "escape-dir")).isSymbolicLink());
  await assertNothingExecuted("checkout");

  // Log: every hostile message and author is visible as literal text.
  await app.click("tab", "History");
  await app.waitText("Layout <script>alert('log r1')</script>", 30_000);
  await app.waitText("<img src=x onerror=alert('log r2')> hostile names");
  await app.waitText("<img src=x onerror=alert('author')>");
  await app.waitText("Invalid UTF-8 �");
  // Raw-markup names show up in the changed paths of r2.
  await clickLast("tab", /^History$/);
  // The revision rows exist twice (sidebar and main list); wait for the main list.
  await clickLast("option", /onerror=alert\('log r2'\)/, 2);
  await app.waitText(`/names/${IMG_NAME}`, 30_000);
  await app.waitText(`/names/${SCRIPT_NAME}`);
  // The markdown/javascript link of r3 is not a link.
  await clickLast("option", /\[click me\]\(javascript:alert\('log r3'\)\)/, 2);
  await app.waitText("/trunk/evil.html", 30_000);
  assert.equal(
    await app.exec("return document.querySelectorAll('a[href^=\"javascript:\" i]').length"),
    0,
  );
  await assertNothingExecuted("history");

  // File list (repository browser) and previews of the active content.
  await app.click("tab", "Browse");
  await app.waitText(TEMPLATE_NAME, 30_000);
  await app.waitText(ENTITY_NAME);
  await app.waitText(PERCENT_NAME);
  // Previews: active content is shown as text. Opening a file moves the browser into it; the
  // breadcrumb button leads back to trunk.
  const preview = async (row, text) => {
    await app.click("option", row);
    await app.waitText(text, 30_000);
    await clickLast("button", /^trunk$/);
    await app.waitText(TEMPLATE_NAME, 30_000);
  };
  await preview(/^evil\.svg/, "<script>alert('svg script'); top.__opensvnPwned = 'svg';</script>");
  await preview(
    /^evil\.html/,
    "<script>alert('html script'); top.__opensvnPwned = 'html';</script>",
  );
  await preview(/^xss-content\.txt/, "<img src=x onerror=alert(1)>");
  await app.click("option", /^escape-abs/);
  await app.waitText("link /etc/passwd", 30_000);
  assert.doesNotMatch(await app.bodyText(), /root:x?:0:0:/, "a symlink preview showed its target");
  // Raw-markup and Windows-invalid names (names/): the engine rejects server names it could not
  // write safely (RelPath::from_server), so the listing shows none of them, only how many it hid
  // (never "empty"); their text is in the History changed paths above.
  await clickLast("treeitem", /^names$/);
  await app.waitText("10 items have names this app cannot use safely and are not shown", 30_000);
  assert.doesNotMatch(await app.bodyText(), /This folder is empty/, "names/ shown as empty");
  await assertNothingExecuted("browse");

  // Properties: local property edits put the hostile values into the diff view as text.
  const svn = (...args) =>
    execFileSync("svn", ["--non-interactive", ...args], { cwd: wc, encoding: "utf8" });
  svn("propdel", "-q", "custom:xss", "props.txt");
  svn("propset", "-q", "custom:img", "changed", "props.txt");
  svn("propdel", "-q", "custom:beacon", "props.txt");
  writeFileSync(join(wc, RLO_NAME), "edited\n");
  // Retarget an escaping symlink (still escaping), so it is listed as a local change.
  rmSync(join(wc, "escape-abs"));
  symlinkSync("/etc/hostname", join(wc, "escape-abs"));
  await app.click("tab", /^Changes\s*\d+$/); // sidebar (with its count)
  await clickLast("tab", /^Changes$/); // main area: the diff
  await app.click("option", /props\.txt/, 30_000);
  await app.waitText("- <script>alert('prop')</script>", 30_000);
  await app.waitText("- <img src=x onerror=alert('prop')>");
  await app.waitText("+ changed");
  await app.waitText("custom:beacon");
  await assertNothingExecuted("properties");

  // Symlink escape, through the UI: reveal and open are refused, nothing is launched.
  await app.click("option", /escape-abs/, 30_000);
  await app.waitText("link /etc/passwd");
  assert.doesNotMatch(await app.bodyText(), /root:x?:0:0:/);
  // The menu acts on every ticked file; untick all so it targets the one row.
  await app.click("checkbox", "Select all files");
  await app.contextMenu("option", /escape-abs/);
  await app.click("menuitem", "Show in file manager");
  await app.waitText("This item is a symbolic link", 15_000);
  await app.contextMenu("option", /escape-abs/);
  await app.click("menuitem", "Open with default app");
  await app.waitFor(
    async () => /not (be )?opened|refused|symbolic link/i.test(await app.bodyText()),
    15_000,
    "open refused",
  );

  // Same commands straight from the page, as injected script would call them.
  const [entry] = (await invoke("wc_list", {})).value;
  const wcId = entry.id;
  for (const path of [
    "escape-abs",
    "escape-dir",
    "escape-dir/passwd",
    "escape-parent",
    "escape-parent/etc",
    "escape-up",
    "into-admin",
    "../../../../etc/passwd",
    "/etc/passwd",
  ]) {
    const reveal = await invoke("reveal_in_os", { wcId, path });
    assert.equal(reveal.ok, false, `reveal_in_os ${path} was accepted`);
    for (const openWith of ["default", "editor", "terminal"]) {
      const open = await invoke("open_external", { wcId, path, openWith });
      assert.equal(open.ok, false, `open_external ${openWith} ${path} was accepted`);
    }
  }
  // A path outside every working copy needs a grant minted by a native dialog; a forged or
  // replayed one is refused.
  for (const grant of ["/etc", "../../etc", "00000000-0000-7000-8000-000000000000"]) {
    const add = await invoke("wc_add_existing", { grant });
    assert.equal(add.ok, false, `wc_add_existing ${grant} was accepted`);
    assert.match(add.error, /no longer valid|Choose it again/);
  }
  await assertNothingExecuted("symlinks");
  assert.equal(readFileSync("/etc/passwd", "utf8").length > 0, true);
});
