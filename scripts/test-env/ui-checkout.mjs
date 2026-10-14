#!/usr/bin/env node
// Drives the OpenSVN app's checkout dialog against the VisualSVN-like server
// (scripts/test-env/visualsvn-like.sh) through tauri-driver + WebKitWebDriver. No npm packages:
// plain WebDriver over HTTP.
//
// Build and run (Linux; needs webkit2gtk-driver, `cargo install tauri-driver`, Xvfb):
//
//   (cd apps/opensvn && pnpm tauri build --debug --no-bundle --features real-engine,negotiate-gssapi)
//   eval "$(scripts/test-env/visualsvn-like.sh env)"
//   Xvfb :99 & DISPLAY=:99 tauri-driver --port 4444 &
//   node scripts/test-env/ui-checkout.mjs target/debug/opensvn
//
// Start tauri-driver with a fresh HOME (and XDG_* below it) for every run and pass the same
// HOME to this script: the app keeps trusted certificates and settings there. With
// OPENSVN_UI_EXPECT_CERT_PROMPT=0 a profile that already trusts the test CA is fine.
// The app runs with a throwaway HOME (fresh settings, default checkout folder inside it) and
// the Kerberos ticket of the environment. Checks:
//  1. the parent-path URL (https://host/svn/) shows the repository list message after the
//     unknown test CA is trusted in the certificate dialog;
//  2. a repository with a Vietnamese name checks out over SSO: no sign-in dialog, the folder
//     name is the decoded repository name, the working copy appears on disk.

import { existsSync, mkdtempSync, readdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const app = resolve(process.argv[2] ?? "target/debug/opensvn");
const driver = process.env.TAURI_DRIVER_URL ?? "http://127.0.0.1:4444";
const host = process.env.OPENSVN_VSVN_HOST ?? "svn.opensvn.test";
const port = process.env.OPENSVN_VSVN_HTTPS_PORT ?? "18443";
const base = `https://${host}:${port}/svn`;
const repoName = "Dự án Tiếng Việt";

const home = mkdtempSync(join(tmpdir(), "opensvn-ui-"));

async function wd(method, path, body) {
  const res = await fetch(`${driver}${path}`, {
    method,
    headers: { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const json = await res.json();
  if (!res.ok) throw new Error(`${method} ${path}: ${JSON.stringify(json.value)}`);
  return json.value;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitFor(what, fn, timeoutMs = 60_000) {
  const end = Date.now() + timeoutMs;
  let last;
  while (Date.now() < end) {
    try {
      const v = await fn();
      if (v) return v;
    } catch (e) {
      last = e;
    }
    await sleep(250);
  }
  throw new Error(`timed out waiting for ${what}${last ? `: ${last.message}` : ""}`);
}

const ELEMENT = "element-6066-11e4-a52e-4f735466cecf";

async function main() {
  const session = await wd("POST", "/session", {
    capabilities: {
      alwaysMatch: {
        "tauri:options": {
          application: app,
          // tauri-driver passes no environment of its own: the app inherits the driver's, so
          // HOME/XDG must be set for the driver process. Recorded here for the log only.
        },
      },
    },
  });
  const sid = session.sessionId;
  const s = (p) => `/session/${sid}${p}`;
  const exec = (script, ...args) => wd("POST", s("/execute/sync"), { script, args });
  const text = () => exec("return document.body.innerText");
  const button = async (label) => {
    // innerText rather than XPath: WebKitWebDriver's XPath misses some labels with "…".
    const el = await exec(
      `return [...document.querySelectorAll('button')]
         .find(b => !b.disabled && b.innerText.trim() === arguments[0]) ?? null;`,
      label,
    );
    return el?.[ELEMENT];
  };
  const click = async (label) => {
    const id = await waitFor(`button "${label}"`, () => button(label));
    await wd("POST", s(`/element/${id}/click`), {});
  };
  const setUrl = async (value) => {
    // React controlled input: use the native setter so onChange fires.
    await exec(
      `const i = document.querySelector('input[list]');
       const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
       set.call(i, arguments[0]);
       i.dispatchEvent(new Event('input', { bubbles: true }));
       return true;`,
      value,
    );
  };
  const results = [];
  const check = (name, ok, detail = "") => {
    results.push({ name, ok, detail });
    console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? ` - ${detail}` : ""}`);
  };

  try {
    await waitFor("welcome screen", async () => (await text()).includes("Checkout repository…"));
    await click("Checkout repository…");
    await waitFor("checkout dialog", async () => (await text()).includes("Repository URL"));

    // 1. Parent path URL.
    await setUrl(`${base}/`);
    await click("Browse…");
    const sawCert = await waitFor(
      "certificate dialog or result",
      async () => {
        const t = await text();
        if (t.includes("Trust certificate")) return "cert";
        if (t.includes("list of repositories")) return "list";
        return null;
      },
      60_000,
    );
    if (process.env.OPENSVN_UI_EXPECT_CERT_PROMPT === "0") {
      console.log(`INFO certificate dialog: ${sawCert === "cert" ? "shown" : "not shown"}`);
    } else {
      check("unknown CA asks for trust (fresh profile)", sawCert === "cert");
    }
    if (sawCert === "cert") await click("Trust permanently");
    const listing = await waitFor("repository list message", async () => {
      const t = await text();
      return t.includes("list of repositories") ? t : null;
    });
    check(
      "parent path URL names the repositories",
      listing.includes("Repositories here") && listing.includes(repoName),
    );

    // 2. Checkout of the Vietnamese repository with SSO.
    await click("Hide folders");
    const url = `${base}/${encodeURIComponent(repoName)}`;
    await setUrl(url);
    const folder = await waitFor("folder name", () =>
      exec(
        `const l = [...document.querySelectorAll('label')]
           .find(x => x.innerText.trim().startsWith('Folder name'));
         const i = l && l.querySelector('input');
         return i ? i.value : null;`,
      ),
    );
    check("folder name is the decoded repository name", folder === repoName, folder);
    await click("Checkout");
    const done = await waitFor(
      "checkout to finish",
      async () => {
        const t = await text();
        if (t.includes("Sign in")) return "prompt";
        if (!t.includes("Repository URL") && !t.includes("Checking out")) return "closed";
        return null;
      },
      300_000,
    );
    check("no sign-in prompt (SSO)", done === "closed", done);
    const wcRoot = await waitFor("working copy on disk", () => {
      const found = [];
      const walk = (dir, depth) => {
        if (depth > 3 || !existsSync(dir)) return;
        for (const e of readdirSync(dir, { withFileTypes: true })) {
          if (!e.isDirectory()) continue;
          const p = join(dir, e.name);
          if (e.name === repoName && existsSync(join(p, ".svn", "wc.db"))) found.push(p);
          else walk(p, depth + 1);
        }
      };
      walk(process.env.HOME ?? home, 0);
      return found[0];
    });
    check("working copy created", Boolean(wcRoot), wcRoot);
    const after = await text();
    check("no error shown after checkout", !/error|failed/i.test(after.split("Activity")[0] ?? ""));
  } finally {
    try {
      const png = await wd("GET", s("/screenshot"));
      const out = join(home, "last.png");
      (await import("node:fs")).writeFileSync(out, Buffer.from(png, "base64"));
      console.log(`screenshot: ${out}`);
    } catch {}
    await wd("DELETE", s(""));
  }
  const failed = results.filter((r) => !r.ok);
  console.log(`${results.length - failed.length}/${results.length} UI checks passed`);
  process.exit(failed.length === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
