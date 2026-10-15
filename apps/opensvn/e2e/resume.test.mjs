// A checkout that loses the network, on the real app, the real engine and Apache +
// mod_dav_svn behind scripts/test-env/flaky-tls-proxy.py (outage mode). See e2e/README.md.
//
// The proxy needs the server certificate and key of scripts/test-env/visualsvn-like.sh next to
// OPENSVN_COMPAT_LOCAL_CA (or OPENSVN_E2E_PROXY_CERTS): `up` writes them.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, openSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { dirname, join } from "node:path";
import { afterEach, beforeEach, test } from "node:test";
import { fileURLToPath } from "node:url";
import { App, PASSWORD, ScratchRepo, sleep, USER } from "./harness.mjs";

const ARTIFACTS = process.env.OPENSVN_E2E_ARTIFACTS ?? join(process.cwd(), "e2e-artifacts");
const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "../../..");

/** @type {App} */
let app;
/** @type {ScratchRepo} */
let repo;
/** @type {import("node:child_process").ChildProcess | null} */
let proxy = null;

async function freePort() {
  return new Promise((ok, fail) => {
    const s = createServer();
    s.unref();
    s.on("error", fail);
    s.listen(0, "127.0.0.1", () => {
      const { port } = s.address();
      s.close(() => ok(port));
    });
  });
}

/** Starts the proxy in front of the server of `repo` with `faults`; resolves to its log path. */
async function startProxy(faults) {
  const certs =
    process.env.OPENSVN_E2E_PROXY_CERTS ??
    dirname(process.env.OPENSVN_COMPAT_LOCAL_CA ?? "/tmp/opensvn-visualsvn/certs/ca.crt");
  const upstream = new URL(repo.server.base);
  const port = await freePort();
  const log = join(app.home, "proxy.log");
  const out = openSync(log, "w");
  proxy = spawn(
    "python3",
    [
      join(repoRoot, "scripts/test-env/flaky-tls-proxy.py"),
      "--listen",
      String(port),
      "--upstream",
      `${upstream.hostname}:${upstream.port || 443}`,
      "--cert",
      join(certs, "server.crt"),
      "--key",
      join(certs, "server.key"),
      "--ca",
      join(certs, "ca.crt"),
      ...faults,
    ],
    { stdio: ["ignore", "ignore", out] },
  );
  await sleep(500);
  const url = new URL(repo.url);
  url.port = String(port);
  return { url: url.toString().replace(/\/+$/, ""), log };
}

beforeEach(async (t) => {
  repo = new ScratchRepo(t.name.replace(/[^a-z0-9]+/gi, "-").slice(0, 20));
  // Large enough that the outage lands in the middle of the checkout.
  const files = {};
  for (let i = 0; i < 300; i++) {
    files[`d${String(Math.floor(i / 50))}/f${i}.txt`] = `${i}\n${"x".repeat(8000)}\n`;
  }
  repo.init(files);
  app = await App.start("resume");
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
  proxy?.kill();
  proxy = null;
  await app.stop();
  repo.dispose();
});

/** Replaces the value of the number input labelled `label` (as settings.test.mjs does). */
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

test("a network outage mid-checkout: Connection lost, Retry now, then the checkout finishes", async () => {
  // A dead connection is noticed after the read timeout: 5 s instead of 60.
  await app.waitText("Welcome to OpenSVN", 30_000);
  await app.click("button", "Settings");
  await app.click("button", "Network");
  await fillNumber("Timeout (seconds, 5–3600)", "5");
  await app.click("button", "Save");
  await app.waitText("Settings saved");
  await app.waitFor(
    () => app.exec(`return !document.querySelector('dialog[aria-label="Settings"]')?.open`),
    10_000,
    "the Settings sheet to close",
  );

  const { url, log } = await startProxy(["--outage-at-bytes", "600000", "--outage-for", "45"]);
  await app.click("button", "Checkout repository…");
  await app.fill("Repository URL", `${url}/trunk`);
  await app.click("button", "Checkout");
  await app.click("button", "Trust permanently", 30_000);
  await app.role("textbox", "Username", 30_000);
  await app.fill("Username", USER);
  await app.fill("Password", PASSWORD);
  await app.click("button", "Sign in");

  // The outage: the dialog says so, with Retry now; the checkout is not failed.
  await app.waitText("Connection lost, retrying", 120_000);
  assert.match(readFileSync(log, "utf8"), /network down/);
  // The Activity panel shows the same notice under the modal dialog: use the dialog's button.
  const clicked = await app.exec(
    `const b = [...document.querySelectorAll('dialog[open] button')].find((x) => x.textContent.trim() === 'Retry now');
     b?.click();
     return !!b;`,
  );
  assert.ok(clicked, "the checkout dialog offers Retry now");
  // Back online: the checkout resumes on its own and finishes.
  await app.waitText(`Checked out ${repo.name}`, 240_000);
  await app.waitText("No local changes", 30_000);
  const parent = join(app.home, "svn");
  const [name] = readdirSync(parent);
  const wc = join(parent, name);
  assert.ok(existsSync(join(wc, ".svn", "wc.db")));
  assert.equal(readFileSync(join(wc, "d5/f299.txt"), "utf8"), `299\n${"x".repeat(8000)}\n`);
  assert.doesNotMatch(await app.bodyText(), /Checkout incomplete/);
});
