#!/usr/bin/env node
// Stamps the release version. Source of truth: [workspace.package] version in the root
// Cargo.toml (RELEASE_VERSION overrides it, for update-path tests). Writes the Tauri config
// override packaging/generated/tauri.version.conf.json and prints the version.
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const SEMVER = /^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$/;

function workspaceVersion() {
  const toml = readFileSync(join(root, "Cargo.toml"), "utf8");
  const section = toml.match(/\[workspace\.package\]([\s\S]*?)(\n\[|$)/);
  const version = section?.[1].match(/^\s*version\s*=\s*"([^"]+)"/m)?.[1];
  if (!version) throw new Error("no version in [workspace.package] of Cargo.toml");
  return version;
}

const version = process.env.RELEASE_VERSION || workspaceVersion();
if (!SEMVER.test(version)) {
  console.error(`error: "${version}" is not a semantic version`);
  process.exit(1);
}

if (!process.env.RELEASE_VERSION) {
  const pkg = JSON.parse(readFileSync(join(root, "apps/opensvn/package.json"), "utf8"));
  if (pkg.version !== version) {
    console.error(
      `error: apps/opensvn/package.json is ${pkg.version} but the workspace is ${version}; bump both`,
    );
    process.exit(1);
  }
}

const gen = join(root, "packaging", "generated");
mkdirSync(gen, { recursive: true });
writeFileSync(join(gen, "tauri.version.conf.json"), `${JSON.stringify({ version }, null, 2)}\n`);
console.log(version);
