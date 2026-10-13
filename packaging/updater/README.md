# Updater design and runbook

OpenSVN updates itself with `tauri-plugin-updater` (pinned `=2.13.3`) from a **private** GitHub
repository, `OpenSVN-releases`, that holds installers and the update manifest only (no source).

## Layout of OpenSVN-releases

| Content | Where | Read through |
|---|---|---|
| `latest.json` (update manifest) | file on branch `main` | Contents API: `https://api.github.com/repos/<owner>/OpenSVN-releases/contents/latest.json?ref=main` with `Accept: application/vnd.github.raw+json` |
| installers, `.sig` files | assets of release `vX.Y.Z` | Release-asset API URL with `Accept: application/octet-stream` |

Both reads are covered by a fine-grained token with **Repository permissions: Contents = Read-only**
on `OpenSVN-releases` and nothing else, so the token cannot reach the source repository.
`latest.json` carries asset-API URLs (`https://api.github.com/repos/<owner>/OpenSVN-releases/releases/assets/<id>`),
which are only known after the assets are uploaded: upload first, then generate the manifest.

## Client side (contract change request for the app owner)

The app does not register the plugin yet; `apps/opensvn/src-tauri` is owned by the app packages, so
this is a request, not part of the packaging change:

1. Root `Cargo.toml` `[workspace.dependencies]`: `tauri-plugin-updater = { version = "=2.13.3", default-features = false, features = ["rustls-tls", "zip"] }`
   (check `cargo deny` stays green; the plugin pulls `reqwest`, `minisign-verify`, `zip`).
2. `apps/opensvn/src-tauri/Cargo.toml`: `tauri-plugin-updater.workspace = true`.
3. `lib.rs`: `.plugin(tauri_plugin_updater::Builder::new().build())` (desktop only).
4. `apps/opensvn/src-tauri/Cargo.toml`: move `serde_json` from `[dev-dependencies]` to
   `[dependencies]`. With `plugins.updater` in the merged config, `tauri::generate_context!()`
   expands to code that names `serde_json` (verified: the build fails with E0433 otherwise).
5. `capabilities/default.json`: `"updater:default"` (or the narrower `updater:allow-check`,
   `updater:allow-download-and-install`).
6. At runtime build the updater with the token, never from config:
   `app.updater_builder().header("Authorization", format!("Bearer {token}"))?.header("Accept", "application/vnd.github.raw+json")?`.
   The token is injected at build time (`option_env!("OPENSVN_UPDATE_TOKEN")`), not committed.
7. Config: release builds merge `packaging/generated/tauri.updater.conf.json`
   (`plugins.updater.pubkey` + `endpoints`) is produced by `scripts/release/write-updater-config.mjs`.

Until then `OPENSVN_UPDATER=1` does not work: the Tauri bundler refuses `createUpdaterArtifacts`
without `plugins.updater` ("plugins > updater doesn't exist"), and with it the app fails to compile
without step 4. `createUpdaterArtifacts` is therefore off by default so that everyone can build
without keys.

## Keys

* Minisign key pair: `pnpm --dir apps/opensvn tauri signer generate -w <path>`. Keep the private
  key and its password in the password manager and, for CI later, in a GitHub environment
  `release` with required reviewers. Only the public key (`*.key.pub`) goes into the app
  (`OPENSVN_UPDATER_PUBKEY`).
* Build-time env: `TAURI_SIGNING_PRIVATE_KEY` (path or content) and
  `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`. Never commit them; `*.key` is git-ignored.
* Apple / Windows code signing is separate and optional, see `docs/deployment-guide.md`.

## Release steps (manual)

1. Bump the version in root `Cargo.toml` (`[workspace.package]`) and `apps/opensvn/package.json`.
2. On each OS run the matching `scripts/release/build-*` with `OPENSVN_UPDATER=1` and the key env.
3. Create release `vX.Y.Z` in `OpenSVN-releases`, upload the installers and `.sig` files.
4. Read the asset IDs (`gh api repos/<owner>/OpenSVN-releases/releases/tags/vX.Y.Z --jq '.assets[]|[.name,.id]'`),
   run `scripts/release/make-latest-json.mjs` with one `--entry platform,artifact,asset-api-url`
   per platform, and commit `latest.json` to `main` of `OpenSVN-releases`.
5. Test N-1 -> N on a machine with the previous build before announcing.

## Token rotation

The token is compiled into each build. To rotate without stranding users:

1. Create the new fine-grained token (same scope, expiry <= 90 days) **before** the old one expires.
2. Ship a release built with the new token; users on the old build update using the old token.
3. Wait until the old version is below the support floor, then delete the old token.
4. Calendar the expiry date one release cycle ahead.

## Incident runbook

* **Token leaked or extracted**: its scope is the installers only (already distributed
  internally). Delete the token, create a new one, ship a release with it. Clients stuck on the
  revoked token must reinstall manually: keep a public download page as fallback.
* **Signing key leaked**: stop publishing, generate a new key pair, publish a release signed with
  the **old** key whose embedded public key is the **new** one (clients trust the old key for this
  hop only), then retire the old key. Announce through the internal channel; tell users to verify
  the SHA-256 of the next installer.
* **Bad release published**: edit `latest.json` on `main` back to the previous version (clients
  only move forward by version, so also publish a fixed higher version quickly) and delete the
  faulty release assets.

## Local test

`scripts/release/test-updater-local.sh` exercises the real plugin against a local static server
(update found, no update, wrong key, tampered artifact, bearer token required). No real
repository or token is involved.
