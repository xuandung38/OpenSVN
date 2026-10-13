#!/usr/bin/env node
// Writes packaging/generated/tauri.updater.conf.json from the template, filling in the public key
// and endpoint from the environment. Nothing secret is read here: the private key only ever
// reaches the Tauri CLI through TAURI_SIGNING_PRIVATE_KEY.
//   OPENSVN_UPDATER_PUBKEY    minisign public key text (the content of the .pub file)
//   OPENSVN_UPDATER_ENDPOINT  manifest URL (default: the private OpenSVN-releases repo)
//   Requires the app to register tauri-plugin-updater and list serde_json as a normal dependency
//   (see packaging/updater/README.md): the Tauri bundler refuses createUpdaterArtifacts without
//   plugins.updater, and tauri::generate_context! then expands to code that names serde_json.
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const template = JSON.parse(
  readFileSync(join(root, "packaging/updater/tauri.updater.conf.json"), "utf8"),
);
const pubkey = process.env.OPENSVN_UPDATER_PUBKEY;
if (!pubkey) {
  console.error("error: OPENSVN_UPDATER_PUBKEY is not set");
  process.exit(1);
}
template.plugins.updater.pubkey = pubkey.trim();
if (process.env.OPENSVN_UPDATER_ENDPOINT) {
  template.plugins.updater.endpoints = [process.env.OPENSVN_UPDATER_ENDPOINT];
}
mkdirSync(join(root, "packaging/generated"), { recursive: true });
writeFileSync(
  join(root, "packaging/generated/tauri.updater.conf.json"),
  `${JSON.stringify(template, null, 2)}\n`,
);
