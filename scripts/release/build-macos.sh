#!/usr/bin/env bash
# Builds the macOS DMG locally.
#
#   scripts/release/build-macos.sh                 arm64 (Apple silicon) DMG, unsigned
#   ARCH=universal scripts/release/build-macos.sh  arm64+x86_64 DMG (needs the x86_64 rust target)
#
# Signing is optional and driven only by environment variables (nothing is ever written to disk):
#   APPLE_SIGNING_IDENTITY   e.g. "Developer ID Application: Name (TEAMID)"; unset = unsigned build
#   APPLE_CERTIFICATE / APPLE_CERTIFICATE_PASSWORD   base64 .p12, for machines without the keychain item
#   notarization (needs a signing identity): APPLE_ID + APPLE_PASSWORD + APPLE_TEAM_ID,
#                            or APPLE_API_ISSUER + APPLE_API_KEY + APPLE_API_KEY_PATH
# Updater artifacts (.app.tar.gz + .sig) are produced when OPENSVN_UPDATER=1 together with
# TAURI_SIGNING_PRIVATE_KEY and OPENSVN_UPDATER_PUBKEY.
# Other knobs: RELEASE_VERSION (override stamped version), SKIP_LICENSES=1 (reuse generated report).
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
[[ "$(uname -s)" == "Darwin" ]] || die "build-macos.sh must run on macOS"
ensure_cargo
need pnpm "pnpm 11"

ARCH="${ARCH:-arm64}"
case "$ARCH" in
  arm64) TRIPLE_ARGS=(); BUNDLE_SUBDIR="release" ;;
  universal)
    rustup target list --installed | grep -q '^x86_64-apple-darwin$' \
      || die "universal needs: rustup target add x86_64-apple-darwin"
    TRIPLE_ARGS=(--target universal-apple-darwin); BUNDLE_SUBDIR="universal-apple-darwin/release" ;;
  *) die "ARCH must be arm64 or universal" ;;
esac

VERSION="$(stamp_version)"
log "version $VERSION, arch $ARCH"

if [[ "${SKIP_LICENSES:-0}" != "1" ]]; then "$REPO_ROOT/scripts/release/gen-licenses.sh"; fi
[[ -f "$GEN_DIR/THIRD-PARTY-LICENSES.txt" ]] || die "run scripts/release/gen-licenses.sh first"

if [[ -n "${APPLE_SIGNING_IDENTITY:-}" ]]; then
  log "signing with identity from APPLE_SIGNING_IDENTITY"
else
  log "no APPLE_SIGNING_IDENTITY: building UNSIGNED (Gatekeeper will quarantine downloads)"
fi

BUNDLES="dmg"
[[ "${OPENSVN_UPDATER:-0}" == "1" ]] && BUNDLES="dmg,app"
tauri_config_args
(cd "$REPO_ROOT" && pnpm install --frozen-lockfile)
(cd "$APP_DIR" && pnpm tauri build --bundles "$BUNDLES" ${TRIPLE_ARGS[@]+"${TRIPLE_ARGS[@]}"} "${CONFIG_ARGS[@]}")

OUT="$GEN_DIR/out/$VERSION"
mkdir -p "$OUT"
BUNDLE_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}/$BUNDLE_SUBDIR/bundle"
[[ -d "$BUNDLE_DIR" ]] || die "bundle directory not found: $BUNDLE_DIR"
cp -f "$BUNDLE_DIR"/dmg/*.dmg "$OUT/"
if [[ "${OPENSVN_UPDATER:-0}" == "1" ]]; then cp -f "$BUNDLE_DIR"/macos/*.app.tar.gz* "$OUT/"; fi
(cd "$OUT" && shasum -a 256 -- *.dmg > SHA256SUMS)
log "artifacts in $OUT"
ls -l "$OUT" >&2
