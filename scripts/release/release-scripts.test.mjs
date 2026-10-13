// Run with: node --test scripts/release/release-scripts.test.mjs
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { buildManifest } from "./make-latest-json.mjs";

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
