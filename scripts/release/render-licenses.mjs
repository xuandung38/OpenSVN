#!/usr/bin/env node
// Merges the cargo-about and pnpm license data from packaging/generated into the shipped
// THIRD-PARTY-LICENSES.txt and the compact third-party-licenses.json.
import { existsSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const gen = process.env.RELEASE_GEN_DIR || join(root, "packaging", "generated");
const rust = JSON.parse(readFileSync(join(gen, "rust-licenses.json"), "utf8"));
const npm = JSON.parse(readFileSync(join(gen, "npm-licenses.json"), "utf8"));

// npm license policy: the cargo-about list (packaging/licenses/about.toml) plus 0BSD and the OFL
// used by the bundled IBM Plex fonts. Anything else (GPL, UNLICENSED, unknown) fails the build.
const NPM_ALLOWED = new Set([
  "Apache-2.0",
  "MIT",
  "BSD-2-Clause",
  "BSD-3-Clause",
  "ISC",
  "Unicode-3.0",
  "Zlib",
  "CC0-1.0",
  "MPL-2.0",
  "0BSD",
  "OFL-1.1",
]);

/** True when an SPDX expression ("MIT", "(MIT OR Apache-2.0)", "A AND B") is acceptable. */
function licenseAllowed(expr) {
  const tokens = expr.replace(/[()]/g, " ").trim().split(/\s+/);
  const anyOr = tokens.includes("OR");
  const ids = tokens.filter((t) => t !== "OR" && t !== "AND");
  if (ids.length === 0) return false;
  return anyOr ? ids.some((id) => NPM_ALLOWED.has(id)) : ids.every((id) => NPM_ALLOWED.has(id));
}

const rejected = Object.keys(npm).filter((l) => !licenseAllowed(l));
if (rejected.length > 0) {
  const names = rejected.flatMap((l) => npm[l].map((p) => `${p.name} (${l})`));
  console.error(`npm license policy violation: ${names.join(", ")}`);
  process.exit(1);
}

const list = [];
const out = [];
const rule = "=".repeat(80);

out.push("OpenSVN third-party licenses", rule, "");
out.push(
  "OpenSVN itself is licensed under the Apache License 2.0 (see LICENSE and NOTICE).",
  "The software below is bundled with it. IBM Plex fonts are under the SIL OFL 1.1",
  "(see ibm-plex-OFL-1.1.txt).",
  "",
);

out.push("RUST CRATES", rule, "");
for (const lic of rust.licenses) {
  const users = lic.used_by.map((u) => u.crate);
  for (const c of users)
    list.push({ ecosystem: "cargo", name: c.name, version: c.version, license: lic.id });
  out.push(`${lic.name} (${lic.id})`, "Used by:");
  for (const c of users)
    out.push(`  - ${c.name} ${c.version}${c.repository ? ` (${c.repository})` : ""}`);
  out.push("", lic.text.trim(), "", "-".repeat(80), "");
}

const LICENSE_FILE = /^(licen[sc]e|copying|notice)(\..*)?$/i;
out.push("NPM PACKAGES (frontend bundle)", rule, "");
for (const [license, pkgs] of Object.entries(npm)) {
  for (const p of pkgs) {
    const version = p.versions?.[0] ?? "";
    list.push({ ecosystem: "npm", name: p.name, version, license });
    out.push(`${p.name} ${version} (${license})${p.author ? ` - ${p.author}` : ""}`);
    const dir = p.paths?.[0];
    const files =
      dir && existsSync(dir) ? readdirSync(dir).filter((f) => LICENSE_FILE.test(f)) : [];
    if (files.length === 0) out.push(`(no license file in package; declared license: ${license})`);
    for (const f of files) out.push("", readFileSync(join(dir, f), "utf8").trim());
    out.push("", "-".repeat(80), "");
  }
}

list.sort((a, b) => a.ecosystem.localeCompare(b.ecosystem) || a.name.localeCompare(b.name));
writeFileSync(join(gen, "THIRD-PARTY-LICENSES.txt"), `${out.join("\n")}\n`);
writeFileSync(join(gen, "third-party-licenses.json"), `${JSON.stringify(list)}\n`);
console.log(`third-party licenses: ${list.length} packages`);
