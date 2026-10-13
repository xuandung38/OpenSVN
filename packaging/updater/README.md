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
`scripts/release/make-latest-json.mjs` refuses any other URL shape (see "Manifest URLs" below).

## The read token lives in the OS keychain, never in the binary

The token is **not** compiled into the app. At runtime the app reads it from the OS keychain
through `secure-store`, under its own key `updater/github-token` (exact service/account naming
follows `secure-store`'s scheme; dev-p08 owns it). Consequences:

* No token in the keychain: the update check is silently disabled and the UI shows a hint
  ("Updates are not configured"). Nothing is sent anywhere.
* Provisioning, once per machine and user: the user (or IT) pastes the token into Settings, or an
  MDM / installer script pre-writes the keychain item. See `docs/deployment-guide.md`.
* Rotation is a re-provisioning of the keychain item, not a release (see below).

## Client side (contract change request for dev-p08)

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
6. Read the token from `secure-store` (key `updater/github-token`); if absent, skip the check and
   surface a hint. Build the updater with it at runtime, never from config or `option_env!`:
   `app.updater_builder().header("Authorization", format!("Bearer {token}"))?.header("Accept", "application/vnd.github.raw+json")?`.
7. **Accept per request.** The plugin reuses the builder headers for the download and only sets
   `application/octet-stream` when no `Accept` is present, so GitHub would answer the asset call
   with metadata JSON and every signature check would fail. After `check()` and before
   downloading, set `update.headers.insert("accept", HeaderValue::from_static("application/octet-stream"))`
   (`Update.headers` is a public `HeaderMap`). The harness does exactly this and
   `scripts/release/test-updater-local.sh` proves both the fix and the failure without it.
8. **Download host check.** The plugin sends the builder headers, including `Authorization`, to
   whatever `platforms.*.url` says and does not validate that URL. Before downloading, reject
   `update.download_url` unless its scheme is `https` and its host is exactly `api.github.com`
   (reference implementation: `--allowed-download-host` in `packaging/updater-harness`). GitHub's
   302 to `objects.githubusercontent.com` is safe: reqwest drops `Authorization` on cross-host
   redirects (verified in `reqwest-0.13.5` `redirect.rs`).
9. Config: release builds merge `packaging/generated/tauri.updater.conf.json`
   (`plugins.updater.pubkey` + `endpoints`), produced by `scripts/release/write-updater-config.mjs`.

Until then `OPENSVN_UPDATER=1` does not work: the Tauri bundler refuses `createUpdaterArtifacts`
without `plugins.updater` ("plugins > updater doesn't exist"), and with it the app fails to compile
without step 4. `createUpdaterArtifacts` is therefore off by default so that everyone can build
without keys.

## Manifest URLs

`make-latest-json.mjs` only accepts
`https://api.github.com/repos/<owner>/OpenSVN-releases/releases/assets/<numeric id>`; anything else
(http, another host, another repo, a browser download URL) is an error. `--allow-any-url` exists
only for the local test script. This is the publishing-side half of the rule; the client-side half
is step 8 above.

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
   The scripts collect only the installer of this version and fail if its `.sig` is missing.
3. Create release `vX.Y.Z` in `OpenSVN-releases`, upload the installers and `.sig` files.
4. Read the asset IDs (`gh api repos/<owner>/OpenSVN-releases/releases/tags/vX.Y.Z --jq '.assets[]|[.name,.id]'`),
   run `scripts/release/make-latest-json.mjs` with one `--entry platform,artifact,asset-api-url`
   per platform, and commit `latest.json` to `main` of `OpenSVN-releases`.
5. Test N-1 -> N on a machine with the previous build before announcing.

## Token rotation

The token is stored per machine in the keychain, so rotating it needs no new release:

1. Create the new fine-grained token (same scope, expiry <= 90 days) before the old one expires.
2. Re-provision the keychain item on every machine: Settings paste for users, the MDM / script
   from the deployment guide for managed fleets.
3. Delete the old token in GitHub once the fleet has switched.
4. Calendar the next expiry. A machine with an expired token simply stops finding updates (the
   check fails quietly and the UI hint points at Settings).

## Incident runbook

* **Token leaked**: its scope is the installers only. Delete it in GitHub, create a new one and
  re-provision. Clients keep working; they just see no updates until re-provisioned.
* **Signing key leaked**: stop publishing, generate a new key pair, publish a release signed with
  the **old** key whose embedded public key is the **new** one (clients trust the old key for this
  hop only), then retire the old key. Announce through the internal channel; tell users to verify
  the SHA-256 of the next installer.
* **Bad release published**: edit `latest.json` on `main` back to the previous version (clients
  only move forward by version, so also publish a fixed higher version quickly) and delete the
  faulty release assets.

## Local test

`scripts/release/test-updater-local.sh` exercises the real plugin against a local static server:
update found, no update, wrong key, tampered artifact, bearer token required, GitHub `Accept`
negotiation (with and without the per-request fix) and the download-host reference check. No real
repository or token is involved.
