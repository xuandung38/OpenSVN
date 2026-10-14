# Deployment guide: building, packaging and updating OpenSVN

Everything here runs **locally**; `.github/workflows/release.yml` runs the same steps on GitHub
Actions and attaches unsigned installers to a draft release (see `docs/INSTALL-TEST.md`). Signing is
optional: without any signing variable set, every script produces a working **unsigned** installer.
Installers are built with the cargo feature `release` (the real engine); `OPENSVN_FEATURES`
overrides the feature list, an empty value builds the fake engine only.

## Layout

| Path | Purpose |
|---|---|
| `apps/opensvn/src-tauri/tauri.bundle.conf.json` | Bundle config merged over `tauri.conf.json` with `--config`: identifier `com.opensvn.desktop`, metadata, license resources, per-OS bundle settings |
| `apps/opensvn/src-tauri/icons/` | Application icons (temporary set) |
| `packaging/macos/entitlements.plist` | Hardened-runtime entitlements (intentionally empty) |
| `packaging/licenses/about.toml` | cargo-about policy for the third-party report |
| `packaging/updater/` | Updater config template and the key/token runbook |
| `packaging/updater-harness/` | Standalone crate that drives the real updater plugin in tests |
| `scripts/release/` | Build, version, license and manifest scripts |
| `packaging/generated/` | Git-ignored output: license report, stamped config, installers in `out/<version>/` |

## Icons

Approved design: blue squircle `#2363d1`, white ring, "SVN" lettering, green `#4cc27e` commit dot.
Sources live in `apps/opensvn/src-tauri/icons/source/`: `opensvn-icon.svg` (master; the lettering is
outlined from Avenir Next Bold, a macOS system font used only to draw the glyphs, so the file needs
no font), `opensvn-icon-1024.png` (rendered master) and `opensvn-icon-small.svg` (ring and dot
only). Regenerate the set from `apps/opensvn`:

```sh
pnpm tauri icon src-tauri/icons/source/opensvn-icon-1024.png   # then delete icons/android and icons/ios
```

The generated 16 px frame is unreadable (the lettering turns to mush and the squircle margin wastes
pixels), so `icon.ico` uses the simplified variant for the 16 and 24 px frames and the master for
32 px and up (lettering is still legible at 32). Build it from freshly rendered 32-bit frames
(`rsvg-convert -w N` of the small SVG for 16/24 and of the master for 32/48/64/256):

```sh
magick PNG32:f16.png PNG32:f24.png PNG32:f32.png PNG32:f48.png PNG32:f64.png PNG32:f256.png icon.ico
```

## Version stamping

The version comes from `[workspace.package] version` in the root `Cargo.toml`.
`scripts/release/stamp-version.mjs` checks that `apps/opensvn/package.json` agrees, then writes
`packaging/generated/tauri.version.conf.json`, which the build scripts merge so the installer and
`latest.json` carry the same number. Set `RELEASE_VERSION=0.2.0` to override (used to produce an
"N" build for update tests).

## Licenses shipped in every installer

Resources copied into the app (`<resources>/licenses/`): `LICENSE` (Apache-2.0), `NOTICE`,
`ibm-plex-OFL-1.1.txt` (IBM Plex fonts, SIL OFL 1.1), `third-party-licenses.json` and `THIRD-PARTY-LICENSES.txt` (306 Rust
crates and 103 npm packages with full texts). The MSI and DMG also show `LICENSE` through
`bundle.licenseFile`. `third-party-licenses.json` next to them is the compact list (`ecosystem`, `name`, `version`,
`license`) for the About screen, which can read it from the resource directory.

Regenerate with `scripts/release/gen-licenses.sh` (needs
`cargo install cargo-about --locked --version 0.9.2 --features cli`). cargo-about fails the run if a
dependency uses a license outside `packaging/licenses/about.toml`; keep it equal to `deny.toml`.
npm packages are checked by `render-licenses.mjs` against the same list plus `0BSD` and `OFL-1.1`
(SPDX `OR` expressions pass when one alternative is allowed); anything else fails the run.

## macOS (DMG)

```sh
scripts/release/build-macos.sh                   # arm64 DMG, unsigned
ARCH=universal scripts/release/build-macos.sh    # arm64 + x86_64
```

Architecture decision for M1: **arm64 only**. The team builds and tests on Apple silicon, the
x86_64 target is not installed by default, and a universal build roughly doubles compile time
with no M1 user needing it. The script already supports `ARCH=universal` (after
`rustup target add x86_64-apple-darwin`); switch to it for the first public release.

Optional signing and notarization, all through environment variables:

```sh
export APPLE_SIGNING_IDENTITY="Developer ID Application: Name (TEAMID)"
export APPLE_ID=you@example.com APPLE_PASSWORD=<app-specific-password> APPLE_TEAM_ID=TEAMID
scripts/release/build-macos.sh
```

(or `APPLE_API_ISSUER`, `APPLE_API_KEY`, `APPLE_API_KEY_PATH` for App Store Connect keys, and
`APPLE_CERTIFICATE` / `APPLE_CERTIFICATE_PASSWORD` to import a `.p12` on a clean machine.)

Installing an unsigned DMG: Gatekeeper blocks it after download. Either right-click the app and
choose Open, or run `xattr -dr com.apple.quarantine /Applications/OpenSVN.app`.

## Linux (AppImage + deb), run on Linux

```sh
sudo apt-get install -y build-essential curl wget file libwebkit2gtk-4.1-dev libgtk-3-dev \
  libayatana-appindicator3-dev librsvg2-dev libssl-dev libxdo-dev patchelf
cargo install cargo-about --locked --version 0.9.2 --features cli
scripts/release/build-linux.sh
```

The deb depends on `libwebkit2gtk-4.1-0`, `libgtk-3-0` and `libgssapi-krb5-2` (the default
`negotiate-gssapi` feature links the Kerberos library). Build on the oldest distro you support: the
AppImage inherits its glibc. Built and smoke-tested on Ubuntu 24.04 (xvfb) for `v0.0.1-test1`.

## Windows (MSI), run on Windows

```powershell
cargo install cargo-about --locked --version 0.9.2 --features cli
powershell -ExecutionPolicy Bypass -File scripts\release\build-windows.ps1
```

WebView2 is installed by the MSI through the evergreen bootstrapper. Optional Authenticode
signing: set `WINDOWS_SIGN_COMMAND` to a command without secrets, preferably a certificate-store
selector (`signtool sign ... /sha1 <thumbprint> "%1"`); see the script header. The command is written to
a git-ignored config file during the build and deleted afterwards, so never put a password in it. Unsigned MSIs trigger SmartScreen:
"More info" then "Run anyway". Not exercised on this branch.

## Updates

See `packaging/updater/README.md` for the design (private `OpenSVN-releases` repo, read-only
fine-grained token, minisign signatures), the release steps, token rotation and incident
runbooks. Test the whole client path offline with `scripts/release/test-updater-local.sh`.
Production endpoints must be `https`: the plugin refuses plain `http` in release builds (the local
test uses a debug build of the harness).

`createUpdaterArtifacts` is off by default so that anyone can build without keys. Once the app
registers the updater plugin (contract change listed in the updater README), set `OPENSVN_UPDATER=1`
plus `TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` and `OPENSVN_UPDATER_PUBKEY`
to produce signed `.app.tar.gz` / `.sig` update artifacts. Until then a build with
`OPENSVN_UPDATER=1` fails by design (verified), and artifacts cannot self-update.

The updater sends the token, so its TLS is never relaxed: `build.rs` fails the build if any
`tauri*.conf.json` sets a `plugins.updater.dangerous*` option (`dangerousAcceptInvalidCerts`,
`dangerousAcceptInvalidHostnames`, `dangerousInsecureTransportProtocol`) to anything but `false`,
and the app refuses to run updates with such a config. Redirects are followed only over `https` to
`api.github.com`, `objects.githubusercontent.com` and `release-assets.githubusercontent.com`; if
GitHub moves release assets to another host, add it to `REDIRECT_HOSTS` in `updater.rs`.

## Upgrade notes

* **Working copy lock owners (Linux).** Before the p08 M2 follow-up, `wc_lock_owner` rows stored
  the owner's start time as wall-clock seconds; they now store ticks since boot. A row written by
  an older build reads as **dead** on Linux, so a lock it describes can be cleaned up as stale.
  This only matters if an old build is still running against the same working copy while the new
  one starts (pre-release: no migration is shipped; quit old builds before upgrading).

## Provisioning the update token

The app never carries the GitHub token. Updates stay disabled (UI hint, no network calls) until
the read-only fine-grained token for `OpenSVN-releases` (Contents: read, see
`packaging/updater/README.md`) is in the OS keychain under the app's key `updater/github-token`
(service and account naming follow `secure-store`; the app side lands with the updater
registration in M2, so confirm the exact names with dev-p08 before scripting a fleet rollout):

* **Single user**: paste the token in Settings > Updates; the app stores it in the keychain.
* **Managed fleet (MDM / login script)**, illustrative commands, adjust the names once fixed:
  macOS `security add-generic-password -s <service> -a updater/github-token -w <token> -U`;
  Windows `cmdkey /generic:<service>:updater/github-token /user:updater /pass:<token>`;
  Linux `secret-tool store --label OpenSVN service <service> account updater/github-token`.
* **Rotation**: write the new token over the old one; no new installer is needed.

## Checklist for an internal release

1. Bump versions (root `Cargo.toml`, `apps/opensvn/package.json`), merge to main.
2. Build on each OS with the scripts above; verify `SHA256SUMS` in `packaging/generated/out/<version>/`.
3. Install on a clean machine, launch, check `licenses/` exists in the resources.
4. Publish per the updater README (when the app registers the updater).
