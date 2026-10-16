// Run with: node --test scripts/release/release-scripts.test.mjs
import assert from "node:assert/strict";
import { execFile, execFileSync } from "node:child_process";
import { chmodSync, mkdtempSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { buildManifest } from "./make-latest-json.mjs";
import {
  compareVersions,
  formatNotes,
  notesFromGit,
  parseArgs,
  previousTag,
  publish,
  updateEntries,
} from "./publish-release.mjs";

const here = dirname(fileURLToPath(import.meta.url));

const ASSET = "https://api.github.com/repos/acme/OpenSVN-releases/releases/assets";

function artifact(sig) {
  const dir = mkdtempSync(join(tmpdir(), "latest-"));
  const file = join(dir, "OpenSVN.app.tar.gz");
  writeFileSync(file, "x");
  writeFileSync(`${file}.sig`, sig);
  return file;
}

test("manifest carries version, signature and url per platform", () => {
  const a = artifact("SIG-A\n");
  const m = buildManifest({
    version: "1.2.3",
    notes: "n",
    entries: [`darwin-aarch64,${a},${ASSET}/42`],
  });
  assert.equal(m.version, "1.2.3");
  assert.deepEqual(m.platforms["darwin-aarch64"], {
    signature: "SIG-A",
    url: `${ASSET}/42`,
  });
  assert.ok(!Number.isNaN(Date.parse(m.pub_date)));
});

test("manifest rejects bad input", () => {
  const a = artifact("S");
  assert.throws(() => buildManifest({ version: "1.2", entries: [`p,${a},https://x`] }));
  assert.throws(() => buildManifest({ version: "1.2.3", entries: [] }));
  assert.throws(() => buildManifest({ version: "1.2.3", entries: [`p,${a},ftp://x`] }));
  assert.throws(() =>
    buildManifest({ version: "1.2.3", entries: [`p,${a},https://x`, `p,${a},https://y`] }),
  );
  assert.throws(() =>
    buildManifest({ version: "1.2.3", entries: [`p,${artifact("  ")},https://x`] }),
  );
});

test("stamp-version follows the workspace version and honours RELEASE_VERSION", () => {
  const cargo = readFileSync(join(here, "..", "..", "Cargo.toml"), "utf8");
  const workspace = cargo.match(/\[workspace\.package\][\s\S]*?version\s*=\s*"([^"]+)"/)[1];
  const out = mkdtempSync(join(tmpdir(), "stamp-"));
  const run = (env) =>
    execFileSync("node", [join(here, "stamp-version.mjs")], {
      env: { ...process.env, RELEASE_GEN_DIR: out, ...env },
      encoding: "utf8",
    }).trim();
  assert.equal(run({ RELEASE_VERSION: "" }), workspace);
  assert.equal(run({ RELEASE_VERSION: "9.8.7-rc.1" }), "9.8.7-rc.1");
  assert.throws(() => run({ RELEASE_VERSION: "nope" }));
});

test("render-licenses rejects npm packages outside the license allowlist", () => {
  const dir = mkdtempSync(join(tmpdir(), "lic-"));
  writeFileSync(join(dir, "rust-licenses.json"), JSON.stringify({ licenses: [] }));
  const run = (npm) => {
    writeFileSync(join(dir, "npm-licenses.json"), JSON.stringify(npm));
    return execFileSync("node", [join(here, "render-licenses.mjs")], {
      env: { ...process.env, RELEASE_GEN_DIR: dir },
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    });
  };
  const pkg = { name: "p", versions: ["1.0.0"], paths: [] };
  run({ MIT: [pkg], "(MIT OR GPL-3.0)": [pkg], "OFL-1.1": [pkg], "0BSD": [pkg] });
  assert.throws(() => run({ "GPL-3.0": [pkg] }), /license policy/);
  assert.throws(() => run({ UNLICENSED: [pkg] }), /license policy/);
  assert.throws(() => run({ "(MIT AND GPL-3.0)": [pkg] }), /license policy/);
});

// ---- publish-release.mjs ----------------------------------------------------------------------

const REL = "acme/OpenSVN-releases";

/**
 * Fake `gh`: logs each call (argv as JSON) to calls.log and answers the few API calls the
 * publisher makes from state.json ({releases: [...], latest: {...} | null}).
 */
function fakeGh(state) {
  const dir = mkdtempSync(join(tmpdir(), "gh-"));
  writeFileSync(join(dir, "state.json"), JSON.stringify(state));
  const bin = join(dir, "gh");
  writeFileSync(
    bin,
    `#!/usr/bin/env node
const fs = require("node:fs"), path = require("node:path");
const dir = ${JSON.stringify(dir)}, a = process.argv.slice(2);
fs.appendFileSync(path.join(dir, "calls.log"), JSON.stringify(a) + "\\n");
const st = JSON.parse(fs.readFileSync(path.join(dir, "state.json"), "utf8"));
const save = () => fs.writeFileSync(path.join(dir, "state.json"), JSON.stringify(st));
if (a[0] === "release" && a[1] === "create") {
  const files = a.filter((x) => x.includes(path.sep) && fs.existsSync(x));
  st.releases.push({ id: 7, tag_name: a[2], draft: true,
    assets: files.map((f, i) => ({ name: path.basename(f), url: "https://api.github.com/repos/${REL}/releases/assets/" + (100 + i) })) });
  save();
} else if (a[0] === "api" && a[1] === "--paginate") {
  const tag = /tag_name==("[^"]+")/.exec(a[4])[1];
  const hit = st.releases.filter((r) => r.tag_name === JSON.parse(tag)).map((r) => ({ id: r.id, assets: r.assets }));
  process.stdout.write(JSON.stringify(hit) + "\\n");
} else if (a[0] === "api" && a[1] === "-X" && a[2] === "PATCH") {
  st.releases.forEach((r) => { r.draft = false; }); save();
} else if (a[0] === "api" && a[1] === "-X" && a[2] === "PUT") {
  const body = JSON.parse(fs.readFileSync(a[a.indexOf("--input") + 1], "utf8"));
  st.put = body; save();
} else if (a[0] === "api" && a[1].includes("contents/latest.json")) {
  if (!st.latest) { process.stderr.write("gh: Not Found (HTTP 404)"); process.exit(1); }
  process.stdout.write(JSON.stringify({ sha: "abc", content: Buffer.from(JSON.stringify(st.latest)).toString("base64") }));
} else { process.stderr.write("unexpected gh call " + a.join(" ")); process.exit(2); }
`,
  );
  chmodSync(bin, 0o755);
  return {
    bin,
    state: () => JSON.parse(readFileSync(join(dir, "state.json"), "utf8")),
    calls: () =>
      readFileSync(join(dir, "calls.log"), "utf8")
        .trim()
        .split("\n")
        .map((l) => JSON.parse(l)),
  };
}

/** A dist directory with the files a full signed release produces (or `names`). */
function dist(names) {
  const d = mkdtempSync(join(tmpdir(), "dist-"));
  for (const n of names) writeFileSync(join(d, n), n.endsWith(".sig") ? `SIG(${n})` : "bin");
  return d;
}
const SIGNED = [
  "OpenSVN.app.tar.gz",
  "OpenSVN.app.tar.gz.sig",
  "OpenSVN_0.1.0_universal.dmg",
  "OpenSVN_0.1.0_x64_en-US.msi",
  "OpenSVN_0.1.0_x64_en-US.msi.sig",
  "OpenSVN_0.1.0_x64-setup.exe",
  "OpenSVN_0.1.0_x64-setup.exe.sig",
  "OpenSVN_0.1.0_amd64.AppImage",
  "OpenSVN_0.1.0_amd64.AppImage.sig",
  "OpenSVN_0.1.0_amd64.deb",
  "SHA256SUMS-linux.txt",
];

function withGh(gh, fn) {
  const saved = { GH_BIN: process.env.GH_BIN, GH_TOKEN: process.env.GH_TOKEN };
  process.env.GH_BIN = gh.bin;
  process.env.GH_TOKEN = "test-token";
  try {
    return fn();
  } finally {
    for (const [k, v] of Object.entries(saved)) {
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
  }
}

const opts = (d, extra = {}) =>
  parseArgs([
    "--tag",
    "v0.1.0",
    "--version",
    "0.1.0",
    "--dist",
    d,
    "--repo",
    REL,
    ...Object.entries(extra).flat(),
  ]);

test("publish: draft + upload, manifest from asset API URLs, publish, then latest.json", () => {
  const gh = fakeGh({ releases: [], latest: null });
  const d = dist(SIGNED);
  const { manifest } = withGh(gh, () => publish(opts(d)));
  const kinds = gh.calls().map((c) => `${c[0]} ${c[1]} ${c[2]}`);
  // Order matters: the manifest is only written after the release is public.
  assert.deepEqual(kinds, [
    "api --paginate repos/acme/OpenSVN-releases/releases?per_page=100",
    "release create v0.1.0",
    "api --paginate repos/acme/OpenSVN-releases/releases?per_page=100",
    "api -X PATCH",
    "api repos/acme/OpenSVN-releases/contents/latest.json?ref=main undefined",
    "api -X PUT",
  ]);
  assert.ok(gh.calls()[1].includes("--draft"));
  assert.ok(!gh.calls()[1].includes("--prerelease"));
  assert.deepEqual(Object.keys(manifest.platforms).sort(), [
    "darwin-aarch64",
    "darwin-x86_64",
    "linux-x86_64",
    "linux-x86_64-appimage",
    "windows-x86_64",
    "windows-x86_64-msi",
    "windows-x86_64-nsis",
  ]);
  for (const p of Object.values(manifest.platforms)) {
    assert.match(
      p.url,
      /^https:\/\/api\.github\.com\/repos\/acme\/OpenSVN-releases\/releases\/assets\/\d+$/,
    );
    assert.match(p.signature, /^SIG\(.+\.sig\)$/);
  }
  assert.equal(
    manifest.platforms["windows-x86_64"].signature,
    "SIG(OpenSVN_0.1.0_x64_en-US.msi.sig)",
  );
  const st = gh.state();
  assert.equal(st.releases[0].draft, false);
  assert.equal(st.releases[0].assets.length, SIGNED.length);
  const put = JSON.parse(Buffer.from(st.put.content, "base64").toString("utf8"));
  assert.equal(put.version, "0.1.0");
  assert.equal(st.put.branch, "main");
  assert.equal(st.put.sha, undefined);
});

test("publish: unsigned build publishes the installers but leaves latest.json alone", () => {
  const unsigned = SIGNED.filter((n) => !n.endsWith(".sig") && !n.endsWith(".app.tar.gz"));
  const gh = fakeGh({ releases: [], latest: null });
  const { manifest } = withGh(gh, () => publish(opts(dist(unsigned))));
  assert.equal(manifest, null);
  assert.equal(gh.state().releases[0].draft, false);
  assert.equal(gh.state().put, undefined);
});

test("publish: an existing release (even a draft) is never overwritten", () => {
  const gh = fakeGh({ releases: [{ id: 1, tag_name: "v0.1.0", draft: true, assets: [] }] });
  assert.throws(() => withGh(gh, () => publish(opts(dist(SIGNED)))), /already exists/);
  assert.equal(gh.calls().length, 1);
});

test("publish: pre-releases and older versions do not move latest.json", () => {
  const pre = fakeGh({ releases: [], latest: null });
  const d = dist(SIGNED);
  const o = parseArgs(["--tag", "v0.1.0-1", "--version", "0.1.0-1", "--dist", d, "--repo", REL]);
  assert.equal(withGh(pre, () => publish(o)).manifest, null);
  assert.ok(pre.calls()[1].includes("--prerelease"));
  assert.equal(pre.state().put, undefined);

  const newer = fakeGh({ releases: [], latest: { version: "0.2.0" } });
  withGh(newer, () => publish(opts(d)));
  assert.equal(newer.state().put, undefined);

  const older = fakeGh({ releases: [], latest: { version: "0.0.9" } });
  withGh(older, () => publish(opts(d)));
  assert.equal(older.state().put.sha, "abc");
});

test("publish: argument and helper validation", () => {
  assert.throws(() => parseArgs(["--tag", "v0.1.1", "--version", "0.1.0", "--dist", "d"]), /--tag/);
  assert.throws(
    () =>
      parseArgs(["--tag", "v0.1.0", "--version", "0.1.0", "--dist", "d", "--repo", "acme/OpenSVN"]),
    /OpenSVN-releases/,
  );
  assert.equal(
    parseArgs(["--tag", "v1.0.0", "--version", "1.0.0", "--dist", "d"]).repo,
    "xuandung38/OpenSVN-releases",
  );
  assert.equal(compareVersions("0.2.0", "0.10.0"), -1);
  assert.equal(compareVersions("1.0.0", "1.0.0-1"), 1);
  assert.equal(compareVersions("1.0.0", "1.0.0"), 0);
  // An update artifact without its .sig is not offered to clients.
  assert.deepEqual(updateEntries("d", ["OpenSVN.app.tar.gz"], new Map()), []);
  assert.throws(() => updateEntries("d", ["a.msi", "a.msi.sig"], new Map()), /no uploaded asset/);
  const gh = fakeGh({ releases: [] });
  withGh(gh, () => {
    delete process.env.GH_TOKEN;
    assert.throws(() => publish(opts(dist(SIGNED))), /GH_TOKEN/);
  });
  assert.equal(gh.state().releases.length, 0);
});

// ---- release notes from git -------------------------------------------------------------------

/** A throw-away repository; `run(...args)` runs git in it with a fixed identity. */
function repo() {
  const dir = mkdtempSync(join(tmpdir(), "notes-"));
  const env = {
    ...process.env,
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_AUTHOR_NAME: "t",
    GIT_AUTHOR_EMAIL: "t@example.com",
    GIT_COMMITTER_NAME: "t",
    GIT_COMMITTER_EMAIL: "t@example.com",
  };
  const run = (...args) => execFileSync("git", args, { cwd: dir, env, encoding: "utf8" }).trim();
  run("init", "-q", "-b", "main");
  const commit = (subject) => run("commit", "-q", "--allow-empty", "-m", subject);
  return { dir, run, commit };
}

test("release notes: grouped by type, merges dropped, previous stable tag as the base", () => {
  const r = repo();
  r.commit("feat: before the previous release");
  r.run("tag", "-a", "v0.1.0", "-m", "0.1.0");
  r.commit("fix(svn-ra): retry a dropped REPORT");
  r.run("tag", "v0.2.0-1");
  r.run("checkout", "-q", "-b", "topic");
  r.commit("feat(ui)!: new settings page");
  r.commit("perf: batch fsync");
  r.run("checkout", "-q", "main");
  r.commit("docs: release checklist");
  r.run("merge", "-q", "--no-ff", "-m", "Merge pull request #1 from acme/topic", "topic");
  r.commit("Keep AppError within the error-size limit");
  r.run("tag", "-a", "v0.2.0", "-m", "0.2.0");
  r.commit("feat: after the release");

  assert.equal(previousTag("0.2.0", "refs/tags/v0.2.0", r.dir), "v0.1.0");
  // A pre-release counts pre-release tags too.
  assert.equal(previousTag("0.2.0-2", "HEAD", r.dir), "v0.2.0-1");
  assert.equal(previousTag("0.1.0", "refs/tags/v0.1.0", r.dir), null);

  const notes = notesFromGit({ tag: "v0.2.0", version: "0.2.0" }, r.dir);
  assert.match(notes, /^OpenSVN 0\.2\.0\. Verify downloads with the SHA256SUMS-\*\.txt files\.\n/);
  assert.doesNotMatch(notes, /Merge pull request|before the previous|after the release/);
  const sections = notes
    .split(/^## /m)
    .slice(1)
    .map((sec) => sec.split("\n")[0]);
  assert.deepEqual(sections, ["Features", "Bug fixes", "Performance", "Other changes"]);
  assert.match(
    notes,
    /## Features\n\n- \*\*BREAKING\*\* \*\*ui:\*\* new settings page \([0-9a-f]{7}\)\n/,
  );
  assert.match(notes, /## Bug fixes\n\n- \*\*svn-ra:\*\* retry a dropped REPORT \(/);
  assert.match(notes, /## Performance\n\n- batch fsync \(/);
  assert.match(notes, /## Other changes\n\n(- .+\n){2}$/);
  assert.match(notes, /- docs: release checklist \(/);
  assert.match(notes, /- Keep AppError within the error-size limit \(/);

  // An untagged version (manual run before tagging) takes HEAD; the first release takes all.
  assert.match(notesFromGit({ tag: "v0.3.0", version: "0.3.0" }, r.dir), /after the release/);
  const first = notesFromGit({ tag: "v0.1.0", version: "0.1.0" }, r.dir);
  assert.match(first, /## Features\n\n- before the previous release/);
});

test("release notes: empty range, unknown types and the size cap", () => {
  assert.match(formatNotes("1.0.0", []), /No changes since the previous release/);
  const odd = formatNotes("1.0.0", [{ hash: "a".repeat(40), subject: "chore(deps): bump" }]);
  assert.match(odd, /## Other changes\n\n- chore\(deps\): bump \(aaaaaaa\)/);
  const many = Array.from({ length: 3000 }, (_, i) => ({
    hash: "b".repeat(40),
    subject: `fix: ${"x".repeat(60)} ${i}`,
  }));
  const big = formatNotes("1.0.0", many);
  assert.ok(big.length < 125_000);
  assert.match(big, /\(list truncated\)\n$/);
  assert.throws(
    () =>
      parseArgs([
        "--tag",
        "v1.0.0",
        "--version",
        "1.0.0",
        "--dist",
        "d",
        "--notes",
        "x",
        "--notes-from-git",
      ]),
    /exclude/,
  );
});

test("release.yml: publish generates the notes from git with full history", () => {
  const release = readFileSync(
    join(here, "..", "..", ".github", "workflows", "release.yml"),
    "utf8",
  );
  const publishJob = jobs(release).publish;
  assert.match(publishJob, /publish-release\.mjs[^\n]*\\\n[^\n]*--notes-from-git/);
  assert.doesNotMatch(publishJob, /publish-release\.mjs[^\n]*\\\n[^\n]*--notes "/);
  assert.match(publishJob, /fetch-depth: 0/);
});

// ---- check-token-scope.sh ---------------------------------------------------------------------

const TOKEN = "github_pat_TESTsecret0123456789";

/** pkt-line framing of the git smart-HTTP protocol. */
const pkt = (line) => `${(line.length + 4).toString(16).padStart(4, "0")}${line}`;

/**
 * A fake api.github.com + github.com that answers per `scope`: which repositories the token can
 * read (API and git) and whether it can write. Requests without the token get 401.
 */
function fakeGitHub(scope) {
  const seen = [];
  const server = createServer((req, res) => {
    const auth = req.headers.authorization ?? "";
    const basic = `Basic ${Buffer.from(`x-access-token:${TOKEN}`).toString("base64")}`;
    seen.push({
      method: req.method,
      url: req.url,
      authed: auth === `Bearer ${TOKEN}` || auth === basic,
    });
    if (!seen.at(-1).authed) return res.writeHead(401).end();
    const reads = (repo) => scope.read.includes(repo);
    let m = /^\/git\/(.+)\.git\/info\/refs\?service=git-upload-pack$/.exec(req.url);
    if (m) {
      if (!reads(m[1])) return res.writeHead(404).end("Repository not found.");
      const sha = "1".repeat(40);
      res.writeHead(200, { "content-type": "application/x-git-upload-pack-advertisement" });
      return res.end(
        `${pkt("# service=git-upload-pack\n")}0000${pkt(`${sha} HEAD\0agent=fake\n`)}${pkt(`${sha} refs/heads/main\n`)}0000`,
      );
    }
    m = /^\/api\/repos\/([^/]+\/[^/]+)(\/contents\/(.+))?$/.exec(req.url);
    if (!m || !reads(m[1])) return res.writeHead(404).end();
    if (req.method === "PUT") return res.writeHead(scope.write ? 404 : 403).end();
    if (m[3] === "latest.json" && !scope.latest) return res.writeHead(404).end();
    return res.writeHead(200).end("{}");
  });
  return new Promise((resolve) =>
    server.listen(0, "127.0.0.1", () => resolve({ server, seen, port: server.address().port })),
  );
}

async function checkScope(scope, env = {}) {
  const gh = await fakeGitHub(scope);
  const base = `http://127.0.0.1:${gh.port}`;
  try {
    return await new Promise((resolve) => {
      execFile(
        join(here, "check-token-scope.sh"),
        [],
        {
          env: {
            ...process.env,
            OPENSVN_UPDATER_TOKEN: TOKEN,
            GITHUB_API: `${base}/api`,
            GITHUB_GIT: `${base}/git`,
            MAIN_REPO: "acme/OpenSVN",
            RELEASES_REPO: "acme/OpenSVN-releases",
            NO_PROXY: "127.0.0.1",
            no_proxy: "127.0.0.1",
            ...env,
          },
          encoding: "utf8",
        },
        (err, stdout, stderr) =>
          resolve({ code: err ? err.code : 0, out: stdout + stderr, seen: gh.seen }),
      );
    });
  } finally {
    gh.server.close();
  }
}

const RELEASES_ONLY = { read: ["acme/OpenSVN-releases"], write: false, latest: true };

test("check-token-scope: a read-only releases token passes, and the token is never printed", async () => {
  const r = await checkScope(RELEASES_ONLY);
  assert.equal(r.code, 0, r.out);
  assert.equal((r.out.match(/^PASS /gm) ?? []).length, 5, r.out);
  assert.match(r.out, /RESULT: PASS/);
  assert.ok(!r.out.includes(TOKEN) && !r.out.includes("TESTsecret"), "token leaked");
  assert.ok(r.seen.every((s) => s.authed));
  assert.ok(r.seen.some((s) => s.method === "PUT"));
  assert.ok(r.seen.some((s) => s.url.startsWith("/git/acme/OpenSVN.git/")));
});

test("check-token-scope: latest.json not published yet is a clear note, not a failure", async () => {
  const r = await checkScope({ ...RELEASES_ONLY, latest: false });
  assert.equal(r.code, 0, r.out);
  assert.match(r.out, /latest\.json -> 404\n\s+note: latest\.json does not exist yet/);
});

test("check-token-scope: a token that reaches the source or can write fails each check", async () => {
  const r = await checkScope({
    read: ["acme/OpenSVN-releases", "acme/OpenSVN"],
    write: true,
    latest: true,
  });
  assert.equal(r.code, 1, r.out);
  assert.match(r.out, /FAIL {2}GET repos\/acme\/OpenSVN -> 200 \(want 404\)/);
  assert.match(r.out, /FAIL {2}git ls-remote acme\/OpenSVN succeeded/);
  assert.match(r.out, /FAIL {2}PUT .* -> 404 \(want 403\)\n.*can write/);
  assert.match(r.out, /RESULT: FAIL/);
  assert.ok(!r.out.includes(TOKEN), "token leaked");
});

test("check-token-scope: a token without the releases repo, a bad token, no token", async () => {
  const none = await checkScope({ read: [], write: false, latest: true });
  assert.equal(none.code, 1, none.out);
  assert.match(none.out, /FAIL {2}GET repos\/acme\/OpenSVN-releases -> 404/);
  assert.match(none.out, /inconclusive: git could not use the token/);

  const bad = await checkScope(RELEASES_ONLY, { OPENSVN_UPDATER_TOKEN: "wrong" });
  assert.equal(bad.code, 1, bad.out);
  assert.match(bad.out, /-> 401\n\s+the token is invalid or expired/);

  const missing = await checkScope(RELEASES_ONLY, { OPENSVN_UPDATER_TOKEN: "" });
  assert.equal(missing.code, 2);
  assert.equal(missing.seen.length, 0);
});

// ---- release.yml secret isolation -------------------------------------------------------------

/** `{ name: text }` of every job under `jobs:` (2-space indented keys), comments stripped. */
function jobs(yml) {
  const body = yml.slice(yml.indexOf("\njobs:\n") + 7);
  const out = {};
  let name = null;
  for (const line of body.split("\n")) {
    const m = /^ {2}([A-Za-z0-9_-]+):\s*$/.exec(line);
    if (m) name = m[1];
    else if (name) out[name] = `${out[name] ?? ""}${line.replace(/(^|\s)#.*$/, "")}\n`;
  }
  return out;
}

test("release.yml: no pull_request trigger, every secret is read only inside environment release", () => {
  const wf = join(here, "..", "..", ".github", "workflows");
  const release = readFileSync(join(wf, "release.yml"), "utf8");
  const on = release.slice(release.indexOf("\non:\n"), release.indexOf("\npermissions:"));
  assert.doesNotMatch(on, /pull_request/);
  const all = jobs(release);
  assert.deepEqual(Object.keys(all).sort(), ["build", "publish"]);
  for (const [name, text] of Object.entries(all)) {
    const secrets = (text.match(/secrets\.[A-Z_]+/g) ?? []).filter(
      (s) => s !== "secrets.GITHUB_TOKEN",
    );
    if (secrets.length)
      assert.match(text, /^ {4}environment: release$/m, `job ${name} reads secrets`);
  }
  assert.match(all.publish, /OPENSVN_RELEASES_TOKEN/);
  // Workflows that run on pull requests must not read any secret.
  for (const f of readdirSync(wf).filter((n) => n !== "release.yml")) {
    const text = readFileSync(join(wf, f), "utf8").replace(/(^|\s)#.*$/gm, "");
    const secrets = (text.match(/secrets\.[A-Z_]+/g) ?? []).filter(
      (s) => s !== "secrets.GITHUB_TOKEN",
    );
    assert.deepEqual(secrets, [], `${f} reads secrets`);
  }
});
