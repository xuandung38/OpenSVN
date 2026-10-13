#!/usr/bin/env bash
# Builds the Linux AppImage and .deb. Run on Linux (Ubuntu 22.04+/24.04 recommended; the
# AppImage inherits the build host's glibc, so build on the oldest distro you support).
#
# Prerequisites (Debian/Ubuntu):
#   sudo apt-get install -y build-essential curl wget file libwebkit2gtk-4.1-dev libgtk-3-dev \
#     libayatana-appindicator3-dev librsvg2-dev libssl-dev libxdo-dev patchelf
#   Node 24 + pnpm 11, Rust via rustup (rust-toolchain.toml pins the compiler), cargo-about.
#
#   scripts/release/build-linux.sh
# No signing certificates are involved; for update artifacts set OPENSVN_UPDATER=1 with
# TAURI_SIGNING_PRIVATE_KEY and OPENSVN_UPDATER_PUBKEY (see packaging/updater/README.md).
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
[[ "$(uname -s)" == "Linux" ]] || die "build-linux.sh must run on Linux"
ensure_cargo
need pnpm "pnpm 11"

VERSION="$(stamp_version)"
log "version $VERSION"
if [[ "${SKIP_LICENSES:-0}" != "1" ]]; then "$REPO_ROOT/scripts/release/gen-licenses.sh"; fi
[[ -f "$GEN_DIR/THIRD-PARTY-LICENSES.txt" ]] || die "run scripts/release/gen-licenses.sh first"

tauri_config_args
(cd "$REPO_ROOT" && pnpm install --frozen-lockfile)
(cd "$APP_DIR" && pnpm tauri build --bundles appimage,deb "${CONFIG_ARGS[@]}")

OUT="$GEN_DIR/out/$VERSION"
mkdir -p "$OUT"
BUNDLE_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}/release/bundle"
[[ -d "$BUNDLE_DIR" ]] || die "bundle directory not found: $BUNDLE_DIR"
cp -f "$BUNDLE_DIR"/deb/*.deb "$BUNDLE_DIR"/appimage/*.AppImage "$OUT/"
if [[ "${OPENSVN_UPDATER:-0}" == "1" ]]; then cp -f "$BUNDLE_DIR"/appimage/*.AppImage.sig "$OUT/" 2>/dev/null || true; fi
(cd "$OUT" && sha256sum -- *.deb *.AppImage > SHA256SUMS)
log "artifacts in $OUT"
