#!/usr/bin/env bash
# Generates the third-party license report that ships in every installer:
#   packaging/generated/THIRD-PARTY-LICENSES.txt   full texts (Rust crates + npm packages)
#   packaging/generated/third-party-licenses.json  compact list for the About screen
# Needs: cargo-about (cargo install cargo-about --locked --version 0.9.2 --features cli), pnpm, node.
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
ensure_cargo
need cargo-about "cargo install cargo-about --locked --version 0.9.2 --features cli"
need pnpm "pnpm 11"
need node "Node 24+"

mkdir -p "$GEN_DIR"
log "Rust dependencies (cargo-about)"
(cd "$REPO_ROOT" && cargo about generate --locked --format json \
  --manifest-path "$TAURI_DIR/Cargo.toml" \
  -c packaging/licenses/about.toml -o "$GEN_DIR/rust-licenses.json")

log "npm dependencies (pnpm licenses)"
(cd "$APP_DIR" && pnpm licenses list --prod --json --long > "$GEN_DIR/npm-licenses.json")

log "rendering"
node "$REPO_ROOT/scripts/release/render-licenses.mjs"
