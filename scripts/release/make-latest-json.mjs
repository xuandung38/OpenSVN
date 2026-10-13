#!/usr/bin/env node
// Builds the Tauri updater manifest (latest.json) from signed update artifacts.
//
//   make-latest-json.mjs --version 0.2.0 --out latest.json [--notes TEXT | --notes-file FILE] \
//       --entry darwin-aarch64,path/OpenSVN.app.tar.gz,https://.../OpenSVN.app.tar.gz \
//       --entry windows-x86_64,path/OpenSVN_0.2.0_x64.msi,https://.../OpenSVN_0.2.0_x64.msi
//
// URLs must be GitHub asset-API URLs of the private releases repository,
//   https://api.github.com/repos/<owner>/OpenSVN-releases/releases/assets/<id>
// because the client sends its read token to whatever host the manifest names. `--allow-any-url`
// lifts the restriction for local tests only.
//
// The signature of each artifact is read from "<artifact>.sig" (written by the Tauri CLI when
// TAURI_SIGNING_PRIVATE_KEY is set). Platform keys: darwin-aarch64, darwin-x86_64,
// linux-x86_64, windows-x86_64 (optionally suffixed -app / -appimage / -msi, see Tauri docs).
import { readFileSync, writeFileSync } from "node:fs";

const ASSET_URL =
  /^https:\/\/api\.github\.com\/repos\/[A-Za-z0-9_.-]+\/OpenSVN-releases\/releases\/assets\/\d+$/;
const SEMVER = /^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$/;

function parse(argv) {
  const o = { entries: [], notes: "" };
  for (let i = 0; i < argv.length; i++) {
    const flag = argv[i];
    const value = () => {
      if (i + 1 >= argv.length) throw new Error(`${flag} needs a value`);
      return argv[++i];
    };
    if (flag === "--version") o.version = value();
    else if (flag === "--out") o.out = value();
    else if (flag === "--notes") o.notes = value();
    else if (flag === "--notes-file") o.notes = readFileSync(value(), "utf8").trim();
    else if (flag === "--pub-date") o.pubDate = value();
    else if (flag === "--entry") o.entries.push(value());
    else if (flag === "--allow-any-url") o.allowAnyUrl = true;
    else throw new Error(`unknown argument ${flag}`);
  }
  return o;
}

export function buildManifest({ version, notes, pubDate, entries, allowAnyUrl = false }) {
  if (!SEMVER.test(version ?? "")) throw new Error(`invalid --version "${version}"`);
  if (entries.length === 0) throw new Error("at least one --entry is required");
  const platforms = {};
  for (const entry of entries) {
    const [platform, artifact, url] = entry.split(",");
    if (!platform || !artifact || !url) throw new Error(`bad --entry "${entry}"`);
    if (platforms[platform]) throw new Error(`duplicate platform ${platform}`);
    const signature = readFileSync(`${artifact}.sig`, "utf8").trim();
    if (!signature) throw new Error(`empty signature: ${artifact}.sig`);
    if (!allowAnyUrl && !ASSET_URL.test(url)) {
      throw new Error(
        `url must be https://api.github.com/repos/<owner>/OpenSVN-releases/releases/assets/<id>: ${url}`,
      );
    }
    if (allowAnyUrl && !/^https?:\/\//.test(url)) throw new Error(`url must be http(s): ${url}`);
    platforms[platform] = { signature, url };
  }
  return { version, notes: notes ?? "", pub_date: pubDate ?? new Date().toISOString(), platforms };
}

if (import.meta.url === `file://${process.argv[1]}`) {
  try {
    const o = parse(process.argv.slice(2));
    if (!o.out) throw new Error("--out is required");
    writeFileSync(o.out, `${JSON.stringify(buildManifest(o), null, 2)}\n`);
    console.log(`wrote ${o.out}`);
  } catch (e) {
    console.error(`error: ${e.message}`);
    process.exit(1);
  }
}
