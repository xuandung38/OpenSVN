#!/usr/bin/env bash
# Shared helpers for the local release scripts. Source this file; do not execute it.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
APP_DIR="$REPO_ROOT/apps/opensvn"
TAURI_DIR="$APP_DIR/src-tauri"
GEN_DIR="$REPO_ROOT/packaging/generated"
export REPO_ROOT APP_DIR TAURI_DIR GEN_DIR

# Keep builds gentle on shared machines unless the caller chose a value.
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"

log() { printf '==> %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1 ($2)"; }

# rust-toolchain.toml pins the compiler; make sure cargo is reachable even when rustup shims are
# not on PATH (common for non-login shells).
ensure_cargo() {
  case ":$PATH:" in *":$HOME/.cargo/bin:"*) ;; *) PATH="$HOME/.cargo/bin:$PATH"; export PATH ;; esac
  if ! command -v cargo >/dev/null 2>&1 && command -v rustup >/dev/null 2>&1; then
    PATH="$(dirname "$(cd "$REPO_ROOT" && rustup which cargo)"):$PATH"
    export PATH
  fi
  need cargo "install Rust via rustup"
}

# Prints the one file in DIR whose name matches the glob PATTERN and that is not older than the
# marker file MARK (touched right before the build). Zero or several matches are a hard error, so
# an installer left over from an earlier version or build can never be packaged by accident.
pick_artifact() {
  local dir="$1" pattern="$2" mark="$3" found=()
  while IFS= read -r f; do found+=("$f"); done \
    < <(find "$dir" -maxdepth 1 -type f -name "$pattern" -newer "$mark" 2>/dev/null)
  [[ ${#found[@]} -eq 1 ]] \
    || die "expected exactly one fresh '$pattern' in $dir, found ${#found[@]}"
  printf '%s\n' "${found[0]}"
}

# With OPENSVN_UPDATER=1 every update artifact needs its .sig: copy both or fail.
copy_with_sig() {
  local src="$1" out="$2"
  cp -f "$src" "$out/"
  if [[ "${OPENSVN_UPDATER:-0}" == "1" ]]; then
    [[ -s "$src.sig" ]] || die "missing signature for $(basename "$src") (OPENSVN_UPDATER=1)"
    cp -f "$src.sig" "$out/"
  fi
}

# Prints the version to stamp (workspace version, or RELEASE_VERSION) and writes the Tauri config
# override that carries it.
stamp_version() {
  need node "Node 24+"
  node "$REPO_ROOT/scripts/release/stamp-version.mjs"
}

# Builds the cargo feature arguments of an installer build. OPENSVN_FEATURES overrides the
# platform default given as $1 (comma-separated; `release` = the real engine); set it to an empty
# string for a build with the fake engine only.
tauri_feature_args() {
  local features="${OPENSVN_FEATURES-$1}"
  FEATURE_ARGS=()
  if [[ -n "$features" ]]; then FEATURE_ARGS+=(--features "$features"); fi
  log "cargo features: ${features:-<none, fake engine only>}"
}

# Builds the --config arguments shared by every platform build. Signing is entirely optional:
# nothing is added unless the matching environment variables are present.
tauri_config_args() {
  CONFIG_ARGS=(
    --config "$TAURI_DIR/tauri.bundle.conf.json"
    --config "$GEN_DIR/tauri.version.conf.json"
  )
  if [[ "${OPENSVN_UPDATER:-0}" == "1" ]]; then
    [[ -n "${TAURI_SIGNING_PRIVATE_KEY:-}" ]] \
      || die "OPENSVN_UPDATER=1 needs TAURI_SIGNING_PRIVATE_KEY (and optionally _PASSWORD)"
    [[ -n "${OPENSVN_UPDATER_PUBKEY:-}" ]] \
      || die "OPENSVN_UPDATER=1 needs OPENSVN_UPDATER_PUBKEY (minisign public key text)"
    node "$REPO_ROOT/scripts/release/write-updater-config.mjs"
    CONFIG_ARGS+=(--config "$GEN_DIR/tauri.updater.conf.json")
  fi
}
