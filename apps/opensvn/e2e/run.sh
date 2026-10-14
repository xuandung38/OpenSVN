#!/bin/sh
# Runs the end-to-end flows (e2e/flows.test.mjs) on Linux: builds the app with the real engine
# unless OPENSVN_E2E_SKIP_BUILD=1, then runs node:test under a virtual X server when no display
# is set. Extra arguments go to `node --test` (e.g. --test-name-pattern="10.2").
# See e2e/README.md for the server and the tools it needs.
set -eu
cd "$(dirname "$0")/.."

for tool in tauri-driver WebKitWebDriver svn; do
  command -v "$tool" >/dev/null 2>&1 || { echo "e2e: '$tool' not found on PATH (see e2e/README.md)" >&2; exit 2; }
done

if [ "${OPENSVN_E2E_SKIP_BUILD:-0}" != 1 ] && [ -z "${OPENSVN_E2E_APP:-}" ]; then
  pnpm tauri build --debug --no-bundle --features real-engine
fi

# `pnpm e2e -- --flag` may hand the separator through.
[ "${1:-}" = "--" ] && shift

export OPENSVN_E2E_ARTIFACTS="${OPENSVN_E2E_ARTIFACTS:-$PWD/e2e-artifacts}"
# Run as a plain script: --test would put the whole file under the per-test timeout too.
set -- node --test-timeout=240000 "$@" e2e/flows.test.mjs
if [ -z "${DISPLAY:-}" ]; then
  exec xvfb-run -a "$@"
fi
exec "$@"
