# PR #59 review: P14 M1 packaging, bundle config, updater scaffolding

Head `f79bfb7d`, base `main`. 42 files, +5331 (4268 of those are `packaging/updater-harness/Cargo.lock`).

**Verdict: REQUEST CHANGES.** The scripts, bundle and DMG work. The blockers are in the updater design: the token is baked into the binary, the `Accept` header design breaks downloads, and manifest URLs are not restricted.

## Verified locally (scratch `git archive`, `CARGO_BUILD_JOBS=2`, cleaned afterwards)
- `node --test scripts/release/release-scripts.test.mjs`: 3/3 pass.
- `scripts/release/test-updater-local.sh`: 6/6 pass against the real `tauri-plugin-updater 2.13.3`:
  - update found, downloaded and signature-verified
  - no update when already current
  - another key's signature rejected
  - tampered artifact rejected
  - request without the token refused (401)
  - request with the bearer token works
- `scripts/release/build-macos.sh` exit 0. It produced `OpenSVN_0.0.0_aarch64.dmg` (5.6 MiB) and `SHA256SUMS`. The license report has 409 packages.
- Mounted the DMG:
  - `Contents/Resources/licenses/` contains LICENSE, NOTICE, `ibm-plex-OFL-1.1.txt`, `THIRD-PARTY-LICENSES.txt` (567 KB) and `third-party-licenses.json`.
  - `CFBundleIdentifier=vn.opensvn.app`, `LSMinimumSystemVersion=12.0`.
  - The app is ad-hoc signed, as expected for an unsigned build.
  - `strings` finds no `ghp_` / `github_pat_` tokens.
- Secrets scan of the diff: clean. The only token-like strings are the throw-away `secret-token` / `test-password` in the local test, and the keys it generates go to git-ignored `packaging/generated/`.
- Upstream source checked (`tauri-plugin-updater-2.13.3`, `reqwest-0.13.5`):
  - Endpoint https enforcement happens only in release builds (`config.rs:198-219`, `#[cfg(not(debug_assertions))]`). The harness is a debug build, which is why http://127.0.0.1 works; that is fine.
  - The **download URL scheme is never validated**: `validate_endpoints` only covers `endpoints`.
  - `check()` defaults `Accept: application/json`, and `download()` defaults `Accept: application/octet-stream`, **but only when no Accept header was set** (`updater.rs:537-539,783-785`).
  - reqwest strips `Authorization` on cross-host redirects (`redirect.rs:239-251`).
- `gh pr checks 59`: every job fails within 2-7 s (GitHub billing block, not code), so CI gives no signal.

## CRITICAL

### C1. The read token is compiled into the binary in plaintext: `packaging/updater/README.md` (Client side step 6, "Token rotation")
The design says "The token is injected at build time (`option_env!("OPENSVN_UPDATE_TOKEN")`)" and "The token is compiled into each build". That violates the requirement that the runtime token live in the OS keychain and never be baked into the binary. Anyone with the installer can extract it with `strings`. The README's own incident runbook admits this ("Token leaked or extracted").

Threat model: the token is read-only on `OpenSVN-releases` (installers only), so the damage is limited. It is still a policy breach, and every leak forces a release plus manual reinstalls.

Fix: the app reads the token from the OS keychain at runtime, through the existing P04 secure store (service e.g. `vn.opensvn.app.updater`). It is provisioned on first run, either by the user or IT pasting it in Settings, or via an MDM/installer step that writes the keychain item. No token means no update check; show "Updates not configured". Remove `option_env!` from the README. Rewrite the rotation runbook: with keychain storage a rotation is a re-provision, not a release.

If the lead prefers zero-touch provisioning over keychain-only, that is a product decision. Present the trade-off (a baked token is extractable but limited in scope; keychain needs provisioning) and get explicit sign-off; don't ship the baked design silently.

## IMPORTANT

### I1. The README's Accept header recipe breaks every download: `packaging/updater/README.md` step 6
Step 6 sets `.header("Accept", "application/vnd.github.raw+json")` on the updater builder. The plugin reuses `self.headers` for `download()` and only adds `application/octet-stream` when no Accept is present (`updater.rs:783-785`). So the asset API call gets `raw+json`, and GitHub returns asset **metadata JSON**, not the binary. Signature verification then fails on every update. The local harness can't catch this because its static server ignores Accept.

Fix: set the Accept header per request. `Update.headers` is a `pub HeaderMap` (`updater.rs:764`), so after `check()` do `update.headers.insert(ACCEPT, "application/octet-stream")` before `download_and_install`. Add a harness case where the static server returns JSON unless `Accept: application/octet-stream`, and another that requires `raw+json` for `latest.json`, so the GitHub semantics are covered.

### I2. The bearer token goes to whatever URL the manifest names, and http is allowed: `scripts/release/make-latest-json.mjs:44`, README step 6
The plugin sends the builder headers, including `Authorization`, to `platforms.*.url` from `latest.json`, and never checks that URL's scheme. `make-latest-json` accepts `http://` URLs. So a typo or a compromised manifest makes the client send the token in cleartext, or to a foreign host. Signature checking still protects integrity, but not the token.

Fix:
1. `make-latest-json` requires `https://api.github.com/repos/<owner>/OpenSVN-releases/releases/assets/<id>` by default, with an explicit `--allow-http` used only by `test-updater-local.sh`.
2. In the app, before downloading, reject `update.download_url` unless its scheme is https and its host is `api.github.com`.
3. Document that GitHub's 302 to `objects.githubusercontent.com` is safe, because reqwest drops `Authorization` on cross-host redirects (verified in source).

### I3. `build-windows.ps1` ignores native-command failures: `scripts/release/build-windows.ps1:22,35-41,62-63`
`$ErrorActionPreference = "Stop"` does not stop on non-zero exit codes from native programs (`cargo about`, `pnpm`, `node`) in Windows PowerShell 5.1, which the script's own `.EXAMPLE` uses (`powershell -ExecutionPolicy Bypass`). If `pnpm tauri build` fails, the script goes on to copy **stale MSIs** from an earlier build into `out/<version>` and hash them.

Fix: after each native call, `if ($LASTEXITCODE -ne 0) { throw "<step> failed" }` (or a small `Invoke-Checked` helper). On PS ≥ 7.4 you can additionally set `$PSNativeCommandUseErrorActionPreference = $true`. Also mirror the bash check that `OPENSVN_UPDATER=1` requires `TAURI_SIGNING_PRIVATE_KEY`.

### I4. Release scripts can package stale or unsigned artifacts
- `build-macos.sh:52` (`*.dmg`) and `build-windows.ps1:68` (`*.msi`) copy every artifact in the shared bundle directory. Older versions' installers end up in `out/<new version>/SHA256SUMS`.
- `build-linux.sh:32` hides a missing `.AppImage.sig` behind `2>/dev/null || true`, so an updater release ships without its signature and nobody notices.

Fix: copy only `*_${VERSION}_*`, and fail when exactly one artifact per bundle type isn't found. With `OPENSVN_UPDATER=1`, fail on a missing `.sig` on every OS (macOS currently relies on `cp` failing, which is fine, but make it explicit).

## MODERATE
- **M1 Ownership.** P14 owns `.github/workflows/release.yml`, `packaging/`, `tauri.bundle.conf.json` and `icons/`. This PR adds `scripts/release/*` (11 files), which is not in any ownership row, and `docs/deployment-guide.md`, which the phase file says goes "PR for lead". Either move the scripts under `packaging/scripts/`, or get the lead to add `scripts/release/` to P14 in `plan.md`. The lead must explicitly accept the doc. No app files are edited: the plugin registration is correctly filed as a contract request in the README.
- **M2 npm licenses are not policy-checked.** `about.toml` gates only Rust crates. `render-licenses.mjs` accepts any npm license, so a GPL or UNLICENSED frontend dependency would ship silently. Today's npm set is fine (MIT 97, ISC, 0BSD, OFL-1.1 fonts, dual MIT/Apache). Fix: an allowlist in `render-licenses.mjs` (the `about.toml` list plus `0BSD` and `OFL-1.1`) that fails on anything else.
- **M3 Copyleft check.** The Rust side has five MPL-2.0 crates: `cssparser`, `cssparser-macros`, `dtoa-short`, `selectors` (tauri/wry HTML parsing) and `option-ext`. MPL-2.0 is file-level weak copyleft. Shipping them unmodified is OK as long as the notice points to their source, which the report's crate repository links do. No GPL, LGPL or AGPL anywhere: no surprises.
- **M4 The 32 px and 48 px ICO entries are 8-bit palette BMPs.** I decoded them: the 16 and 24 px entries are 32 bpp and use the small variant (ring and dot, no lettering), which satisfies the small-variant requirement. The 32 px entry is 8 bpp with a 1-bit mask, and its anti-aliased corners show palette fringing (stray dark and white pixels). 32 and 48 are the Explorer and taskbar sizes. Regenerate them as 32 bpp, e.g. `pnpm tauri icon source/opensvn-icon-1024.png`, then re-insert the small variant at 16/24.
- **M5 Identifier change.** `tauri.bundle.conf.json` sets `vn.opensvn.app`, while `tauri.conf.json` has `dev.opensvn.app`. The release identifier is effectively permanent: it fixes the app-data dir, keychain scoping and the updater identity. The lead should confirm `vn.opensvn.app` and check that P04/P08 derive their keychain service and data dir from the runtime identifier, not a hard-coded string.
- **M6 Windows sign command.** `build-windows.ps1:12` shows `/p <password>` inside `WINDOWS_SIGN_COMMAND`, and `:49-51` writes that command to `packaging/generated/tauri.sign.windows.conf.json`, which contradicts "nothing is ever written to disk". Recommend a certificate-store selector (`/sha1 <thumbprint>`) or Azure Trusted Signing in the example, and delete the generated file after the build.
- **M7 The updater harness is outside `cargo deny`.** It is a standalone workspace with its own `Cargo.lock`, and is not shipped. Fine for now, but when the app adopts the plugin, `reqwest`, `zip` and `minisign-verify` enter the shipped graph, and `cargo deny` must stay green (the README already notes this).

## LOW
- The stamp-version test writes `packaging/generated/tauri.version.conf.json` with `9.8.7-rc.1` as a side effect. Harmless, because every build re-stamps, but it should use a temp dir.
- `test-updater-local.sh` matches `"available":true` against stdout+stderr combined. Matching stdout only would be stricter.
- The local test doesn't cover release-mode https rejection or the GitHub Accept semantics. That is covered by I1 and I2.

## Checklist answers
1. **Secrets**: nothing committed. Signing keys and the Apple/Windows credentials come from env only. **The read token design embeds it in the binary (C1)**.
2. **Updater security**: signature verification is enforced (tested). https is enforced for endpoints in release builds only, and **not for download URLs (I2)**. No downgrade: the default comparator is `remote > current`, and the harness uses the same `>`. The bearer token is **not** limited to the configured host (I2).
3. **Bundling**: LICENSE, NOTICE, OFL and the third-party report are all inside the .app (verified in the DMG). No copyleft surprises (M3). npm licenses have no policy gate (M2).
4. **Scripts**: the bash scripts use `set -euo pipefail` (via `common.sh`), are well quoted, and have no curl|sh. The PowerShell script has error-handling gaps (I3). Artifact globbing is unsafe (I4).
5. **Icons**: the full Tauri set is regenerated, including Windows Store logos, icns and SVG sources. The ICO has the small variant at 16 and 24 px. Its 32/48 px entries have quality issues (M4).
6. **Ownership**: `scripts/release/` and `docs/deployment-guide.md` are outside the P14 paths (M1). No engine or app code is touched.

## Unresolved questions
1. The lead needs to decide between keychain-provisioned tokens (requirement) and a baked token (zero-touch) for C1.
2. Confirm `vn.opensvn.app` as the permanent release identifier (M5).
3. Should `scripts/release/` be added to P14's ownership, or moved under `packaging/`?

Status: DONE_WITH_CONCERNS
Summary: Packaging works locally (DMG with all license files, 6/6 updater checks, node tests pass). REQUEST CHANGES because the updater design embeds the read token in the binary (C1), its Accept header recipe would break every real GitHub download (I1), and the token can go to non-https or foreign manifest URLs (I2).

---

# Re-review: head `5164725` (P14 commits `61b034e`, `5164725`; the rest is a main merge)

**Verdict: APPROVE.**

Verified locally (scratch copy, `CARGO_BUILD_JOBS=2`, cleaned afterwards):
- `node --test`: 4/4 pass, including the new npm license allowlist test (GPL, UNLICENSED and `MIT AND GPL` rejected).
- `test-updater-local.sh`: 12/12 pass. Every rejection fails for the intended reason:
  - wrong key, and tampered artifact (signature errors)
  - missing token
  - manifest fetched without `raw+json` (unparseable)
  - `--download-accept keep` (signature verification failed, which proves the I1 pitfall)
  - foreign host (`download host … is not api.github.com`)
  - http download URL (`not https`)
- `build-macos.sh` exit 0. `pick_artifact` collected only `OpenSVN_0.0.0_aarch64.dmg`, and SHA256SUMS has one line. The DMG contains all 5 license files and `CFBundleIdentifier=com.opensvn.desktop`.
- ICO: all 6 entries are now 32 bpp.

| Item | Status |
|---|---|
| C1 baked token | Fixed (design/docs). The token lives in the keychain via `secure-store` under `updater/github-token`; no token means no check plus a UI hint; `option_env!` removed; rotation = re-provision. The app side is owned by dev-p08 and still has to land and be reviewed. |
| I1 Accept | Fixed. README step 7 + harness insert `octet-stream` after `check()`; the static server mimics GitHub negotiation. |
| I2 URL/token scope | Fixed. `make-latest-json` only accepts `https://api.github.com/repos/<owner>/OpenSVN-releases/releases/assets/<id>` (`--allow-any-url` for tests only); README step 8 = app-side https + exact-host check (reference in the harness). |
| I3 PowerShell | Fixed. `Invoke-Checked` on every native call; the sign file is removed in `finally`; `OPENSVN_UPDATER` key check. Not run, no Windows host. |
| I4 stale artifacts | Fixed. `pick_artifact` (exactly one file newer than the pre-build marker); `copy_with_sig` fails on a missing `.sig`; Windows equivalent uses `LastWriteTime`. |
| M2 npm policy | Fixed. |
| M4 ICO | Fixed. |
| M5 identifier | `com.opensvn.desktop` per PO decision, applied to both `tauri.bundle.conf.json` and `tauri.conf.json`. |
| M6 sign command | Fixed. Thumbprint example; no password; transient file. |
| M1 ownership | Still open. `scripts/release/` and `docs/deployment-guide.md` sit outside the P14 paths, and `tauri.conf.json` (app-owned) is now edited for the identifier. The lead should accept these explicitly. |

Remaining (non-blocking):
- `expect_fail` in the local test does not assert the error text; I checked the reasons by hand above. Asserting a substring per case would keep the test honest.
- The SPDX check treats any expression that contains `OR` as "some allowed id". That is fine for today's packages, but nested `(A AND B) OR C` is only approximated.
- CI is still blocked by GitHub billing, and the Linux/Windows scripts were not run.

Status: DONE
Summary: All C1/I1-I4 and moderate items are fixed and verified locally (12/12 updater checks, DMG with all licenses, 4/4 node tests). APPROVE; ownership acceptance (M1) and the dev-p08 keychain implementation are still open.
