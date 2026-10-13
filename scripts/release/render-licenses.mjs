#!/usr/bin/env node
// Merges the cargo-about and pnpm license data from packaging/generated into the shipped
// THIRD-PARTY-LICENSES.txt and the compact third-party-licenses.json.
import { existsSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const gen = join(root, "packaging", "generated");
const rust = JSON.parse(readFileSync(join(gen, "rust-licenses.json"), "utf8"));
const npm = JSON.parse(readFileSync(join(gen, "npm-licenses.json"), "utf8"));

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
