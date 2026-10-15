#!/bin/sh
# Runs end-to-end tests on Linux (default e2e/flows.test.mjs; OPENSVN_E2E_FILE picks another, e.g.
# e2e/hostile.test.mjs or e2e/crash.test.mjs): builds the app with the real engine unless
# OPENSVN_E2E_SKIP_BUILD=1 (OPENSVN_E2E_FEATURES adds cargo features, e.g. `failpoints` for the
# crash test; OPENSVN_E2E_RELEASE=1 builds a release binary, for the memory tests), then runs
# node:test under a virtual X server when no display is set. OPENSVN_E2E_TIMEOUT is the per-test
# timeout in ms (default 300000). Extra arguments go to `node --test` (e.g.
# --test-name-pattern="10.2").
# See e2e/README.md for the server and the tools it needs.
set -eu
cd "$(dirname "$0")/.."

for tool in tauri-driver WebKitWebDriver svn; do
  command -v "$tool" >/dev/null 2>&1 || { echo "e2e: '$tool' not found on PATH (see e2e/README.md)" >&2; exit 2; }
done

features="real-engine${OPENSVN_E2E_FEATURES:+,$OPENSVN_E2E_FEATURES}"
if [ "${OPENSVN_E2E_SKIP_BUILD:-0}" != 1 ] && [ -z "${OPENSVN_E2E_APP:-}" ]; then
  if [ "${OPENSVN_E2E_RELEASE:-0}" = 1 ]; then
    pnpm tauri build --no-bundle --features "$features"
  else
    pnpm tauri build --debug --no-bundle --features "$features"
  fi
fi
if [ "${OPENSVN_E2E_RELEASE:-0}" = 1 ] && [ -z "${OPENSVN_E2E_APP:-}" ]; then
  OPENSVN_E2E_APP="$(cd ../.. && pwd)/target/release/opensvn"
  export OPENSVN_E2E_APP
fi

# `pnpm e2e -- --flag` may hand the separator through.
[ "${1:-}" = "--" ] && shift

export OPENSVN_E2E_ARTIFACTS="${OPENSVN_E2E_ARTIFACTS:-$PWD/e2e-artifacts}"
# Run as a plain script: --test would put the whole file under the per-test timeout too.
set -- node --test-timeout="${OPENSVN_E2E_TIMEOUT:-300000}" "$@" "${OPENSVN_E2E_FILE:-e2e/flows.test.mjs}"
if [ -z "${DISPLAY:-}" ]; then
  exec xvfb-run -a "$@"
fi
exec "$@"
