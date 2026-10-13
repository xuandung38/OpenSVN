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

# Prints the version to stamp (workspace version, or RELEASE_VERSION) and writes the Tauri config
# override that carries it.
stamp_version() {
  need node "Node 24+"
  node "$REPO_ROOT/scripts/release/stamp-version.mjs"
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
