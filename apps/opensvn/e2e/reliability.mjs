// Shared pieces of the app-level reliability tests (crash.test.mjs, rss.test.mjs, soak.test.mjs):
// a Secret Service for the app's saved credentials, checkout and reopening through the UI, the
// svn command line client of the oracle, a seeded random source and a memory sampler.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { PASSWORD, sleep, USER } from "./harness.mjs";

export const ARTIFACTS = process.env.OPENSVN_E2E_ARTIFACTS ?? join(process.cwd(), "e2e-artifacts");

/** svn command line client used as the oracle (`OPENSVN_SVN_BIN`, else `svn` on PATH). */
export const SVN = process.env.OPENSVN_SVN_BIN ?? "svn";

/** Banners that would ask the user to repair the working copy by hand. */
export const REPAIR_BANNERS = [
  "Working copy is locked",
  "A previous operation did not finish.",
  "Run cleanup",
];

/**
 * A private D-Bus session with an unlocked gnome-keyring (Secret Service), so the app keeps the
 * password it is given across a relaunch as on a desktop. Without one (no `dbus-launch` or
 * `gnome-keyring-daemon`) the app would ask to sign in again after every restart.
 */
export function startKeyring() {
  const home = mkdtempSync(join(tmpdir(), "opensvn-e2e-keyring-"));
  const base = { ...process.env, HOME: home, XDG_DATA_HOME: join(home, "data") };
  const out = execFileSync("dbus-launch", ["--sh-syntax"], { env: base, encoding: "utf8" });
  const address = out.match(/DBUS_SESSION_BUS_ADDRESS='([^']+)'/)?.[1];
  const busPid = Number(out.match(/DBUS_SESSION_BUS_PID=(\d+)/)?.[1]);
  if (!address || !busPid) throw new Error(`dbus-launch: unexpected output ${out}`);
  const env = { DBUS_SESSION_BUS_ADDRESS: address };
  execFileSync("gnome-keyring-daemon", ["--unlock", "--components=secrets"], {
    env: { ...base, ...env },
    input: "e2e-keyring",
    stdio: ["pipe", "ignore", "ignore"],
  });
  return {
    env,
    stop() {
      // gnome-keyring-daemon leaves with its bus.
      try {
        process.kill(busPid, "SIGTERM");
      } catch {
        // already gone
      }
      // The bus may have started the document portal, which mounts a FUSE folder there.
      for (const tool of ["fusermount3", "fusermount"]) {
        try {
          execFileSync(tool, ["-u", "-z", join(home, ".cache/doc")], { stdio: "ignore" });
          break;
        } catch {
          // not mounted, or this tool is missing
        }
      }
      try {
        rmSync(home, { recursive: true, force: true });
      } catch {
        // best effort: a temp folder
      }
    },
  };
}

/** Working copy folder a checkout created (the default parent is ~/svn of the profile). */
export function wcRoot(app) {
  const parent = join(app.home, "svn");
  const [name] = readdirSync(parent);
  return join(parent, name);
}

/** Checkout through the UI: URL, trust the certificate, sign in, wait for the empty list. */
export async function checkout(app, repo, path = "trunk", timeout = 120_000) {
  await app.click("button", "Checkout repository…", 30_000);
  await app.fill("Repository URL", `${repo.url}/${path}`);
  await app.click("button", "Checkout");
  await app.click("button", "Trust permanently", 30_000);
  await app.role("textbox", "Username", 30_000);
  await app.fill("Username", USER);
  await app.fill("Password", PASSWORD);
  await app.click("button", "Sign in");
  await app.waitText(`Checked out ${repo.name}`, timeout);
  await app.waitText("No local changes", timeout);
  return wcRoot(app);
}

/**
 * After a relaunch: waits for the working copy `name` to be the current one (the app reopens the
 * last one), opening it from the repository list when it is not.
 */
export async function reopen(app, name, timeout = 60_000) {
  await app.waitFor(
    async () => {
      const text = await app.bodyText();
      if (new RegExp(`Current repository\\s+${name}`).test(text)) return true;
      // Not current yet: pick it like a user would.
      const el = await app.exec(
        `return [...document.querySelectorAll('[role="option"],button,a')].find((e) => e.innerText.trim().split('\\n')[0] === arguments[0]) ?? null`,
        name,
      );
      if (el) await app.s.click(el["element-6066-11e4-a52e-4f735466cecf"]);
      return false;
    },
    timeout,
    `working copy ${name} to be current`,
  );
}

/** Text of the banners that ask for a manual repair, if any is shown. */
export async function repairBanners(app) {
  const text = await app.bodyText();
  return REPAIR_BANNERS.filter((b) => text.includes(b));
}

/** Runs the oracle svn client (non-interactive, the test account) and returns stdout. */
export function svn(args, cwd) {
  return execFileSync(
    SVN,
    [
      "--non-interactive",
      "--no-auth-cache",
      "--username",
      USER,
      "--password",
      PASSWORD,
      "--trust-server-cert-failures=unknown-ca,cn-mismatch",
      ...args,
    ],
    { cwd, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"], maxBuffer: 256 << 20 },
  );
}

/** `svn status` lines as `[column text, path]`, ignoring the externals/summary noise. */
export function svnStatus(wc) {
  return svn(["status", "--ignore-externals"], wc)
    .split("\n")
    .filter((l) => l.length > 8 && !l.startsWith("Performing status"))
    .map((l) => [l.slice(0, 8), l.slice(8)]);
}

/** Youngest revision of the repository at `url`. */
export function youngest(url) {
  return Number(svn(["info", "--show-item", "revision", url]).trim());
}

/**
 * Fills `repo` (trunk/branches/tags) with `count` text files named by `fileName(i)` straight on
 * the server's disk (file://, then handed to Apache's user): `svn import` of 100k files over
 * HTTPS would take most of the run. Needs a local server (`OPENSVN_COMPAT_LOCAL_*`).
 */
export function seedLarge(repo, count, fileName) {
  assert.equal(repo.server.kind, "local", "needs a local server (OPENSVN_COMPAT_LOCAL_*)");
  const tree = mkdtempSync(join(tmpdir(), "opensvn-e2e-tree-"));
  try {
    for (const d of ["trunk", "branches", "tags"]) mkdirSync(join(tree, d), { recursive: true });
    let lastDir = "";
    for (let i = 0; i < count; i++) {
      const p = join(tree, "trunk", fileName(i));
      const dir = join(p, "..");
      if (dir !== lastDir) mkdirSync(dir, { recursive: true });
      lastDir = dir;
      writeFileSync(p, `file ${i}\n${"line of text\n".repeat(20)}`);
    }
    const dir = join(repo.server.repos, repo.name);
    const owner = execFileSync("stat", ["-c", "%u:%g", dir], { encoding: "utf8" }).trim();
    execFileSync(SVN, ["import", "-q", "-m", "Large import", tree, `file://${dir}`]);
    execFileSync("chown", ["-R", owner, dir]);
  } finally {
    rmSync(tree, { recursive: true, force: true });
  }
}

/** One commit by another user without a working copy: `files` maps repository URL -> text. */
export function commitUrls(repo, message, files) {
  const args = ["-m", message];
  let n = 0;
  for (const [url, text] of Object.entries(files)) {
    const f = join(repo.work, `mucc-${Date.now()}-${n++}.txt`);
    writeFileSync(f, text);
    args.push("put", f, url);
  }
  execFileSync("svnmucc", [
    "--non-interactive",
    "--no-auth-cache",
    "--username",
    USER,
    "--password",
    PASSWORD,
    "--trust-server-cert-failures=unknown-ca,cn-mismatch",
    ...args,
  ]);
}

/** Invokes a backend command from the page, as the UI does; resolves to `{ ok }` or `{ err }`. */
export function invoke(app, cmd, args) {
  return app.s.execAsync(
    `const [cmd, args, done] = arguments;
     window.__TAURI_INTERNALS__.invoke(cmd, args).then((v) => done({ ok: v }), (e) => done({ err: e }));`,
    cmd,
    args,
  );
}

/**
 * Runs backend command `cmd` that starts an operation and waits for that operation to end, by
 * listening to `op://finished` / `op://failed` in the page (as `@tauri-apps/api/event` does).
 * Resolves to `{ finished }` or `{ failed }`.
 */
export async function runOperation(app, cmd, args, timeout = 600_000) {
  await app.s.execAsync(
    `const done = arguments[0];
     if (window.__e2eOps) return done(true);
     window.__e2eOps = {};
     const t = window.__TAURI_INTERNALS__;
     const on = (event, key) => t.invoke("plugin:event|listen", {
       event,
       target: { kind: "Any" },
       handler: t.transformCallback((e) => { window.__e2eOps[e.payload.opId] = { [key]: e.payload }; }),
     });
     Promise.all([on("op://finished", "finished"), on("op://failed", "failed")]).then(() => done(true), (e) => done(String(e)));`,
  );
  const started = await invoke(app, cmd, args);
  assert.ok(started.ok, `${cmd}: ${JSON.stringify(started.err)}`);
  const opId = started.ok.opId;
  return app.waitFor(
    () => app.exec("return window.__e2eOps[arguments[0]] ?? null", opId),
    timeout,
    `${cmd} (${opId}) to end`,
  );
}

/** Variables for the app's environment from `"K=V K2=V2"` in environment variable `name`. */
export function appEnvFrom(name) {
  return Object.fromEntries(
    (process.env[name] ?? "")
      .split(/\s+/)
      .filter(Boolean)
      .map((kv) => [kv.slice(0, kv.indexOf("=")), kv.slice(kv.indexOf("=") + 1)]),
  );
}

/** Deterministic random source (mulberry32) for replayable runs. */
export function rng(seed) {
  let a = seed >>> 0;
  const next = () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
  return {
    next,
    int: (n) => Math.floor(next() * n),
    pick: (list) => list[Math.floor(next() * list.length)],
  };
}

/** Samples the app's process tree memory every `everyMs` until stopped; keeps the peaks. */
export class MemorySampler {
  constructor(app, everyMs = 250) {
    this.app = app;
    this.samples = [];
    this.peak = { rssKb: 0, pssKb: 0, appRssKb: 0, pids: 0 };
    this.phase = "start";
    this.phasePeaks = new Map();
    this.timer = setInterval(() => this.sample(), everyMs);
  }

  /** Samples `app` from now on (a relaunch on the same profile); the peaks carry over. */
  follow(app) {
    this.app = app;
  }

  /** Names the current step; peaks are also kept per step. */
  mark(phase) {
    this.sample();
    this.phase = phase;
  }

  sample() {
    const m = this.app.treeMemory();
    if (!m) return null;
    this.samples.push({ t: Date.now(), phase: this.phase, ...m });
    for (const k of ["rssKb", "pssKb", "appRssKb", "pids"])
      this.peak[k] = Math.max(this.peak[k], m[k]);
    const p = this.phasePeaks.get(this.phase) ?? { rssKb: 0, pssKb: 0, appRssKb: 0 };
    for (const k of ["rssKb", "pssKb", "appRssKb"]) p[k] = Math.max(p[k], m[k]);
    this.phasePeaks.set(this.phase, p);
    return m;
  }

  stop() {
    clearInterval(this.timer);
    this.sample();
  }
}

/** Megabytes, one decimal. */
export const mb = (kb) => Math.round(kb / 102.4) / 10;

/** Writes `text` below the artifacts folder and returns its path. */
export function writeArtifact(name, text) {
  mkdirSync(ARTIFACTS, { recursive: true });
  const p = join(ARTIFACTS, name);
  writeFileSync(p, text);
  return p;
}

/** Screenshot, window text and driver log of a failed test (best effort). */
export async function saveFailure(app, name) {
  mkdirSync(ARTIFACTS, { recursive: true });
  const base = join(ARTIFACTS, name.replace(/[^a-z0-9]+/gi, "-"));
  try {
    await app.screenshot(`${base}.png`);
    writeFileSync(`${base}.txt`, `${await app.bodyText()}\n\n--- driver log ---\n${app.log()}`);
  } catch {
    // the app may be gone
  }
}

/** Waits until `file` exists (a failpoint marker) or the timeout passes; true when it appeared. */
export async function waitForFile(exists, timeout) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    if (exists()) return true;
    await sleep(20);
  }
  return false;
}
