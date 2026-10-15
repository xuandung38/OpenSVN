#!/usr/bin/env node
// Publishes a built release to the separate releases repository (installers + latest.json).
//
//   publish-release.mjs --tag v0.1.0 --version 0.1.0 --dist dist [--repo owner/OpenSVN-releases]
//       [--notes TEXT] [--latest auto|always|never]
//
// Needs GH_TOKEN: a fine-grained token whose only permission is Contents: Read and write on the
// releases repository. Steps, in an order that never lets a client see a manifest pointing at
// missing assets:
//   1. refuse if the release already exists (a published version is never overwritten)
//   2. create the release as a draft and upload every file in --dist
//   3. read the asset API URLs and build latest.json from the update artifacts and their .sig
//   4. publish the release (draft -> released)
//   5. commit latest.json to `main` of the releases repository
// latest.json is only written when update artifacts with signatures exist, and (with
// --latest auto, the default) only for versions without a pre-release suffix, and never moves
// the version backwards. GH_BIN overrides the gh executable (tests).
import { execFileSync } from "node:child_process";
import { mkdtempSync, readdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { buildManifest } from "./make-latest-json.mjs";

const SEMVER = /^(\d+)\.(\d+)\.(\d+)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$/;
const REPO = /^[A-Za-z0-9_.-]+\/OpenSVN-releases$/;

/** Update artifacts (matched by file name) and the manifest platform keys each one serves. */
const UPDATE_ARTIFACTS = [
  // The macOS build is universal, so one archive serves both architectures.
  { match: /\.app\.tar\.gz$/, platforms: ["darwin-aarch64", "darwin-x86_64"] },
  // The plain key is what older clients and unknown installer types fall back to: MSI.
  { match: /\.msi$/, platforms: ["windows-x86_64-msi", "windows-x86_64"] },
  { match: /-setup\.exe$/, platforms: ["windows-x86_64-nsis"] },
  { match: /\.AppImage$/, platforms: ["linux-x86_64-appimage", "linux-x86_64"] },
];

export function parseArgs(argv) {
  const o = { latest: "auto", notes: "" };
  for (let i = 0; i < argv.length; i++) {
    const flag = argv[i];
    const value = () => {
      if (i + 1 >= argv.length) throw new Error(`${flag} needs a value`);
      return argv[++i];
    };
    if (flag === "--tag") o.tag = value();
    else if (flag === "--version") o.version = value();
    else if (flag === "--dist") o.dist = value();
    else if (flag === "--repo") o.repo = value();
    else if (flag === "--notes") o.notes = value();
    else if (flag === "--latest") o.latest = value();
    else throw new Error(`unknown argument ${flag}`);
  }
  o.repo ??= "xuandung38/OpenSVN-releases";
  if (!SEMVER.test(o.version ?? "")) throw new Error(`invalid --version "${o.version}"`);
  if (o.tag !== `v${o.version}`) throw new Error(`--tag must be v${o.version}, got "${o.tag}"`);
  if (!REPO.test(o.repo)) throw new Error(`--repo must be <owner>/OpenSVN-releases: ${o.repo}`);
  if (!o.dist) throw new Error("--dist is required");
  if (!["auto", "always", "never"].includes(o.latest))
    throw new Error("--latest: auto|always|never");
  return o;
}

/** Compares two x.y.z[-pre] versions by their numeric core; a pre-release sorts first. */
export function compareVersions(a, b) {
  const pa = SEMVER.exec(a);
  const pb = SEMVER.exec(b);
  if (!pa || !pb) throw new Error(`cannot compare "${a}" and "${b}"`);
  for (let i = 1; i <= 3; i++) {
    const d = Number(pa[i]) - Number(pb[i]);
    if (d !== 0) return Math.sign(d);
  }
  if (!pa[4] !== !pb[4]) return pa[4] ? -1 : 1;
  return (pa[4] ?? "").localeCompare(pb[4] ?? "");
}

/**
 * Manifest entries (`platform,path,url`) for the update artifacts in `files`, given the asset
 * API URL of each uploaded file name. Every update artifact must have a non-empty `.sig`.
 */
export function updateEntries(dist, files, assetUrls) {
  const entries = [];
  for (const name of files) {
    const kind = UPDATE_ARTIFACTS.find((k) => k.match.test(name));
    if (!kind) continue;
    if (!files.includes(`${name}.sig`)) continue;
    const url = assetUrls.get(name);
    if (!url) throw new Error(`no uploaded asset for ${name}`);
    for (const p of kind.platforms) entries.push(`${p},${join(dist, name)},${url}`);
  }
  return entries;
}

function gh(args, opts = {}) {
  return execFileSync(process.env.GH_BIN || "gh", args, {
    encoding: "utf8",
    stdio: ["pipe", "pipe", "pipe"],
    ...opts,
  });
}

/** The release (draft or not) with tag `o.tag`, as `{id, assets: [{name, url}]}`, or null. */
function findRelease(o) {
  const jq = `[.[]|select(.tag_name==${JSON.stringify(o.tag)})|{id,assets:[.assets[]|{name,url}]}]`;
  const pages = gh(["api", "--paginate", `repos/${o.repo}/releases?per_page=100`, "--jq", jq]);
  const found = pages
    .split("\n")
    .filter(Boolean)
    .flatMap((l) => JSON.parse(l));
  return found[0] ?? null;
}

const notice = (msg) => console.log(`::notice::${msg}`);

export function publish(o) {
  if (!process.env.GH_TOKEN) throw new Error("GH_TOKEN is not set");
  const files = readdirSync(o.dist).sort();
  if (files.length === 0) throw new Error(`nothing to publish in ${o.dist}`);
  const prerelease = SEMVER.exec(o.version)[4] !== undefined;

  // The list (unlike GET releases/tags/{tag}) includes drafts, so a draft left behind by a failed
  // run is found too.
  if (findRelease(o)) {
    throw new Error(
      `release ${o.tag} already exists in ${o.repo} (maybe a draft from a failed run: delete it, ` +
        "or bump the version)",
    );
  }
  const create = ["release", "create", o.tag, "--repo", o.repo, "--draft", "--title", o.tag];
  create.push("--notes", o.notes || `OpenSVN ${o.version}`);
  if (prerelease) create.push("--prerelease");
  gh([...create, ...files.map((f) => join(o.dist, f))]);
  console.log(`created draft release ${o.tag} in ${o.repo} with ${files.length} files`);

  const release = findRelease(o);
  if (!release) throw new Error(`draft release ${o.tag} not found after creating it`);
  // `url` of an asset is its API URL (.../releases/assets/<id>), the shape latest.json needs.
  const urls = new Map(release.assets.map((a) => [a.name, a.url]));
  const missing = files.filter((f) => !urls.has(f));
  if (missing.length) throw new Error(`assets missing after upload: ${missing.join(", ")}`);

  const entries = updateEntries(o.dist, files, urls);
  let manifest = null;
  if (entries.length === 0) {
    notice("no signed update artifacts: latest.json is not written (updater signing is off)");
  } else if (o.latest === "never" || (o.latest === "auto" && prerelease)) {
    notice(`latest.json left unchanged for ${o.version} (--latest ${o.latest})`);
  } else {
    manifest = buildManifest({ version: o.version, notes: o.notes, entries });
  }

  // Publishing the draft creates the tag on the releases repository's default branch.
  gh(["api", "-X", "PATCH", `repos/${o.repo}/releases/${release.id}`, "-F", "draft=false"]);
  console.log(`published release ${o.tag}`);

  if (manifest) writeLatest(o, manifest);
  return { files, manifest };
}

function writeLatest(o, manifest) {
  let sha;
  try {
    const current = JSON.parse(gh(["api", `repos/${o.repo}/contents/latest.json?ref=main`]));
    sha = current.sha;
    const old = JSON.parse(Buffer.from(current.content, "base64").toString("utf8"));
    if (compareVersions(old.version, manifest.version) >= 0) {
      notice(`latest.json already announces ${old.version}; not moving it to ${manifest.version}`);
      return;
    }
  } catch (e) {
    if (!/404|Not Found/.test(`${e.stderr ?? ""}${e.message}`)) throw e;
  }
  const body = {
    message: `latest.json: ${manifest.version}`,
    branch: "main",
    content: Buffer.from(`${JSON.stringify(manifest, null, 2)}\n`).toString("base64"),
    ...(sha ? { sha } : {}),
  };
  const file = join(mkdtempSync(join(tmpdir(), "latest-")), "body.json");
  writeFileSync(file, JSON.stringify(body));
  gh(["api", "-X", "PUT", `repos/${o.repo}/contents/latest.json`, "--input", file]);
  console.log(`latest.json now announces ${manifest.version}`);
}

if (import.meta.url === `file://${process.argv[1]}`) {
  try {
    const o = parseArgs(process.argv.slice(2));
    publish(o);
  } catch (e) {
    console.error(`::error::${e.stderr ? `${e.message}: ${e.stderr}` : e.message}`);
    process.exit(1);
  }
}
