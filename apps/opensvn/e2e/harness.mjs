// End-to-end harness: a scratch repository on Apache + mod_dav_svn (HTTPS, Basic auth), the real
// OpenSVN binary (built with the real engine) started fresh per test through tauri-driver, and
// helpers that find controls the way a user does (role + visible name).
//
// Server selection (same variables as tests/real_engine_dav.rs):
// - OPENSVN_COMPAT_LOCAL_URL (parent-path URL, e.g. https://localhost:18276/svn) and
//   OPENSVN_COMPAT_LOCAL_REPOS (its SVNParentPath, writable by the test);
// - otherwise the compat container `opensvn-compat-svn15-server-1` on https://localhost:8444
//   (`cargo xtask compat up`).
// The server must accept tester / tester-pass (Basic); its certificate is trusted in the app's
// dialog, like a user would.

import { execFileSync, spawn } from "node:child_process";
import {
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  readlinkSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { Session } from "./webdriver.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "../../..");
export const USER = "tester";
export const PASSWORD = "tester-pass";

/** Binary under test: OPENSVN_E2E_APP or the debug build of `tauri build --debug`. */
export const APP = process.env.OPENSVN_E2E_APP ?? join(repoRoot, "target/debug/opensvn");

function server() {
  const base = process.env.OPENSVN_COMPAT_LOCAL_URL;
  const repos = process.env.OPENSVN_COMPAT_LOCAL_REPOS;
  if (base && repos) return { kind: "local", base: base.replace(/\/+$/, ""), repos };
  return {
    kind: "docker",
    base: "https://localhost:8444/svn",
    container: "opensvn-compat-svn15-server-1",
  };
}

let counter = 0;

/** A scratch repository with `trunk/`, `branches/`, `tags/`; removed by `dispose()`. */
export class ScratchRepo {
  constructor(tag) {
    this.server = server();
    this.name = `e2e-${tag}-${process.pid}-${counter++}`;
    this.url = `${this.server.base}/${this.name}`;
    this.svnadmin(["create"]);
    this.work = mkdtempSync(join(tmpdir(), "opensvn-e2e-seed-"));
    this.seed = join(this.work, "seed");
  }

  svnadmin(args) {
    if (this.server.kind === "local") {
      // A wrapper that runs svnadmin as Apache's user (scripts/test-env/visualsvn-like.sh as root).
      const svnadmin = process.env.OPENSVN_COMPAT_LOCAL_SVNADMIN ?? "svnadmin";
      execFileSync(svnadmin, [...args, join(this.server.repos, this.name)]);
    } else {
      execFileSync("docker", [
        "exec",
        "-u",
        "33:33",
        this.server.container,
        "svnadmin",
        ...args,
        `/var/svn/repos/${this.name}`,
      ]);
    }
  }

  /** Runs the svn command line client as another user's tool would (same account). */
  svn(args, cwd = this.work) {
    return execFileSync(
      "svn",
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
      { cwd, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
    );
  }

  /** Imports `files` (relative path -> text) below `trunk/` as r1, plus branches/ and tags/. */
  init(files) {
    const tree = join(this.work, "import");
    for (const d of ["trunk", "branches", "tags"]) mkdirSync(join(tree, d), { recursive: true });
    for (const [rel, text] of Object.entries(files)) {
      const p = join(tree, "trunk", rel);
      mkdirSync(dirname(p), { recursive: true });
      writeFileSync(p, text);
    }
    this.svn(["import", "-q", "-m", "Initial import", tree, this.url]);
    this.svn(["checkout", "-q", `${this.url}/trunk`, this.seed]);
  }

  /** Writes files in the seed working copy and commits them as someone else would. */
  commitFromElsewhere(files, message = "Change from elsewhere") {
    this.svn(["update", "-q"], this.seed);
    for (const [rel, text] of Object.entries(files)) {
      const p = join(this.seed, rel);
      mkdirSync(dirname(p), { recursive: true });
      writeFileSync(p, text);
      try {
        this.svn(["add", "-q", "--parents", rel], this.seed);
      } catch {
        // already versioned
      }
    }
    this.svn(["commit", "-q", "-m", message], this.seed);
  }

  dispose() {
    try {
      if (this.server.kind === "local") {
        rmSync(join(this.server.repos, this.name), { recursive: true, force: true });
      } else {
        execFileSync("docker", [
          "exec",
          "-u",
          "33:33",
          this.server.container,
          "rm",
          "-rf",
          `/var/svn/repos/${this.name}`,
        ]);
      }
    } catch {
      // best effort
    }
    rmSync(this.work, { recursive: true, force: true });
  }
}

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

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** The app under test with its own home, data and config folders. */
export class App {
  /**
   * `extraEnv` is merged last (e.g. a `PATH` with recording stand-ins for OS launchers). `home`
   * reuses the profile of an earlier run (relaunch after a crash); `stop({ keepHome: true })` or
   * `kill()` leaves it in place.
   */
  static async start(tag, extraEnv = {}, { home } = {}) {
    home ??= mkdtempSync(join(tmpdir(), `opensvn-e2e-${tag}-`));
    const env = {
      ...process.env,
      HOME: home,
      XDG_DATA_HOME: join(home, "data"),
      XDG_CONFIG_HOME: join(home, "config"),
      XDG_CACHE_HOME: join(home, "cache"),
      OPENSVN_ENGINE: "real",
      // No accessibility bus or keyring in CI containers; GTK must not wait for them.
      NO_AT_BRIDGE: "1",
      ...extraEnv,
    };
    for (const d of ["data", "config", "cache"]) mkdirSync(join(home, d), { recursive: true });
    const port = await freePort();
    const nativePort = await freePort();
    const driver = spawn(
      process.env.TAURI_DRIVER ?? "tauri-driver",
      ["--port", String(port), "--native-port", String(nativePort)],
      // Own process group: stop() ends the driver, WebKitWebDriver and the app together.
      { env, stdio: ["ignore", "pipe", "pipe"], detached: true },
    );
    let log = "";
    driver.stdout.on("data", (d) => (log += d));
    driver.stderr.on("data", (d) => (log += d));
    const base = `http://127.0.0.1:${port}`;
    const deadline = Date.now() + 20_000;
    for (;;) {
      try {
        const r = await fetch(`${base}/status`);
        if (r.ok) break;
      } catch {
        // not up yet
      }
      if (Date.now() > deadline) throw new Error(`tauri-driver did not start:\n${log}`);
      await sleep(100);
    }
    const session = await Session.create(base, {
      "tauri:options": { application: APP },
    });
    const app = new App(session, driver, home, () => log);
    await app.waitFor(
      () =>
        app.exec("return document.readyState === 'complete' && !!document.body.innerText.trim()"),
      30_000,
      "app window",
    );
    return app;
  }

  constructor(session, driver, home, log) {
    this.s = session;
    this.driver = driver;
    this.home = home;
    this.log = log;
  }

  exec(script, ...args) {
    return this.s.exec(script, ...args);
  }

  async stop({ keepHome = false } = {}) {
    try {
      await this.s.delete();
    } catch {
      // the app may already be gone
    }
    await this.endGroup();
    if (!keepHome) rmSync(this.home, { recursive: true, force: true });
  }

  /**
   * Kills the app's process group (driver, WebKitWebDriver, the app and its WebKit processes)
   * with SIGKILL, like a crash or a power cut: nothing gets to clean up. The profile stays.
   */
  async kill() {
    try {
      process.kill(-this.driver.pid, "SIGKILL");
    } catch {
      // already gone
    }
    const deadline = Date.now() + 10_000;
    while (groupPids(this.driver.pid).length && Date.now() < deadline) await sleep(50);
    this.driver.stdout.destroy();
    this.driver.stderr.destroy();
    this.driver.unref();
  }

  /** Pid of the app binary (a child of WebKitWebDriver in the driver's process group). */
  appPid() {
    return groupPids(this.driver.pid).find((pid) => exeOf(pid) === APP) ?? null;
  }

  /** Resident memory of the app and every process below it (WebKit web/network processes). */
  treeMemory() {
    const root = this.appPid();
    if (root === null) return null;
    const pids = [root, ...descendants(root)];
    let rssKb = 0;
    let pssKb = 0;
    for (const pid of pids) {
      rssKb += statusKb(pid, "VmRSS");
      pssKb += rollupKb(pid, "Pss");
    }
    return { pids: pids.length, rssKb, pssKb, appRssKb: statusKb(root, "VmRSS") };
  }

  /** Ends the driver's process group: SIGTERM, then SIGKILL. */
  async endGroup() {
    // An app the session did not close would keep the driver's pipes, and this process, alive.
    for (const signal of ["SIGTERM", "SIGKILL"]) {
      try {
        process.kill(-this.driver.pid, signal);
      } catch {
        break; // the group is gone
      }
      await sleep(500);
    }
    this.driver.stdout.destroy();
    this.driver.stderr.destroy();
    this.driver.unref();
  }

  /** Polls `fn` until it returns a truthy value. */
  async waitFor(fn, timeout = 15_000, what = "condition") {
    const deadline = Date.now() + timeout;
    let last;
    for (;;) {
      try {
        const v = await fn();
        if (v) return v;
      } catch (e) {
        last = e;
      }
      if (Date.now() > deadline) {
        throw new Error(`timed out waiting for ${what}${last ? `: ${last.message}` : ""}`);
      }
      await sleep(150);
    }
  }

  /** Visible text of the whole window. */
  bodyText() {
    return this.exec("return document.body.innerText");
  }

  async waitText(text, timeout = 15_000) {
    return this.waitFor(
      async () => (await this.bodyText()).includes(text),
      timeout,
      `text ${JSON.stringify(text)}`,
    );
  }

  /**
   * Element with `role` whose accessible name (aria-label, else text) matches `name` (string:
   * exact after trimming; RegExp: test). Only visible elements count.
   */
  async role(role, name, timeout = 15_000) {
    const pattern = name instanceof RegExp ? { source: name.source, flags: name.flags } : null;
    const exact = typeof name === "string" ? name : null;
    return this.waitFor(
      () =>
        this.exec(
          `const [role, exact, pattern] = arguments;
           const implicit = { button: "button", link: "a[href]", textbox: "input:not([type]),input[type=text],input[type=password],input[type=url],input[type=search],textarea", checkbox: "input[type=checkbox]", radio: "input[type=radio]", combobox: "select", dialog: "dialog" };
           const sel = '[role="' + role + '"]' + (implicit[role] ? ',' + implicit[role] : '');
           const re = pattern ? new RegExp(pattern.source, pattern.flags) : null;
           const nameOf = (el) => {
             const l = el.getAttribute('aria-label');
             if (l) return l.trim();
             const by = el.getAttribute('aria-labelledby');
             if (by) return by.split(/\\s+/).map((id) => document.getElementById(id)?.innerText ?? '').join(' ').trim();
             if (el.id) { const lab = document.querySelector('label[for="' + el.id + '"]'); if (lab) return lab.innerText.trim(); }
             const wrap = el.closest('label'); if (wrap && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA')) return wrap.innerText.trim();
             return (el.innerText ?? el.textContent ?? '').trim();
           };
           const visible = (el) => { const r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0; };
           return [...document.querySelectorAll(sel)].find((el) => visible(el) && (exact !== null ? nameOf(el) === exact : re.test(nameOf(el)))) ?? null;`,
          role,
          exact,
          pattern,
        ),
      timeout,
      `${role} ${String(name)}`,
    );
  }

  async click(role, name, timeout) {
    const el = await this.role(role, name, timeout);
    await this.s.click(el["element-6066-11e4-a52e-4f735466cecf"]);
  }

  /** Opens the right-click menu of the element (a synthetic `contextmenu` event at its centre). */
  async contextMenu(role, name, timeout) {
    const el = await this.role(role, name, timeout);
    await this.exec(
      `const el = arguments[0]; const r = el.getBoundingClientRect();
       el.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true, button: 2, clientX: r.x + r.width / 2, clientY: r.y + r.height / 2 }));`,
      el,
    );
  }

  /** Replaces the value of the text box named `name` with `text`. */
  async fill(name, text) {
    const el = await this.role("textbox", name);
    const id = el["element-6066-11e4-a52e-4f735466cecf"];
    await this.s.clear(id);
    await this.s.type(id, text);
  }

  async keys(...chord) {
    await this.s.keys(chord);
  }

  async screenshot(path) {
    const png = await this.s.screenshot();
    writeFileSync(path, Buffer.from(png, "base64"));
  }
}

/** Live processes (not zombies) of process group `pgid`. */
function groupPids(pgid) {
  const out = [];
  for (const name of readdirSync("/proc")) {
    if (!/^\d+$/.test(name)) continue;
    const stat = procStat(name);
    if (stat && stat.pgrp === pgid && stat.state !== "Z") out.push(Number(name));
  }
  return out;
}

/** Fields of /proc/<pid>/stat after the command name. */
function procStat(pid) {
  try {
    const raw = readFileSync(`/proc/${pid}/stat`, "utf8");
    const f = raw.slice(raw.lastIndexOf(")") + 2).split(" ");
    return { state: f[0], ppid: Number(f[1]), pgrp: Number(f[2]) };
  } catch {
    return null;
  }
}

function exeOf(pid) {
  try {
    return readlinkSync(`/proc/${pid}/exe`);
  } catch {
    return null;
  }
}

function descendants(root) {
  const children = new Map();
  for (const name of readdirSync("/proc")) {
    if (!/^\d+$/.test(name)) continue;
    const stat = procStat(name);
    if (!stat || stat.state === "Z") continue;
    if (!children.has(stat.ppid)) children.set(stat.ppid, []);
    children.get(stat.ppid).push(Number(name));
  }
  const out = [];
  const queue = [root];
  while (queue.length) {
    for (const c of children.get(queue.shift()) ?? []) {
      out.push(c);
      queue.push(c);
    }
  }
  return out;
}

function statusKb(pid, field) {
  try {
    const m = readFileSync(`/proc/${pid}/status`, "utf8").match(
      new RegExp(`^${field}:\\s+(\\d+) kB`, "m"),
    );
    return m ? Number(m[1]) : 0;
  } catch {
    return 0;
  }
}

function rollupKb(pid, field) {
  try {
    const m = readFileSync(`/proc/${pid}/smaps_rollup`, "utf8").match(
      new RegExp(`^${field}:\\s+(\\d+) kB`, "m"),
    );
    return m ? Number(m[1]) : 0;
  } catch {
    return 0;
  }
}

export { sleep };
