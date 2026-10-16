#!/usr/bin/env bash
# Runs secure-store's `os_store_roundtrip` (set, get, delete) against a real Secret Service on
# Linux without a desktop session: a private D-Bus session bus (dbus-run-session) and a
# gnome-keyring-daemon whose login keyring is created and unlocked with a throwaway password.
#
#   scripts/test-env/secret-service-roundtrip.sh install   # apt packages (Debian/Ubuntu, needs root)
#   scripts/test-env/secret-service-roundtrip.sh           # run the test
#
# Extra arguments go to cargo (`--release`, `--locked` ...). The keyring lives in a temporary
# HOME that is deleted afterwards; the user's own keyring is never touched.
set -euo pipefail

die() { echo "secret-service-roundtrip: $*" >&2; exit 1; }

if [ "${1:-}" = install ]; then
  if [ "$(id -u)" = 0 ]; then sudo=(); else sudo=(sudo); fi
  "${sudo[@]}" env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    dbus dbus-x11 gnome-keyring
  exit 0
fi

command -v dbus-run-session >/dev/null || die "dbus-run-session not found (run: $0 install)"
command -v gnome-keyring-daemon >/dev/null || die "gnome-keyring-daemon not found (run: $0 install)"

# Re-run inside a private session bus; it and the daemons started on it end with this script.
if [ -z "${OPENSVN_SECRET_SERVICE_BUS:-}" ]; then
  exec env OPENSVN_SECRET_SERVICE_BUS=1 dbus-run-session -- "$0" "$@"
fi

root="$(cd "$(dirname "$0")/../.." && pwd)"
# cargo and rustup keep using the real home; only the keyring gets a throwaway one.
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
tmp="$(mktemp -d)"
# The daemon exits with the private session bus when dbus-run-session ends.
trap 'rm -rf "$tmp"' EXIT
mkdir -m 700 "$tmp/run"
export HOME="$tmp" XDG_DATA_HOME="$tmp/data" XDG_RUNTIME_DIR="$tmp/run"

# `--unlock` reads the login keyring password on stdin and creates the keyring when it does not
# exist yet; `--start` then attaches the Secret Service component to this session bus.
printf '%s' "opensvn-test-only" | gnome-keyring-daemon --unlock --components=secrets >/dev/null
gnome-keyring-daemon --start --components=secrets >/dev/null 2>&1
echo "secret-service-roundtrip: $(gnome-keyring-daemon --version | head -n1) on ${DBUS_SESSION_BUS_ADDRESS%%,*}"

cd "$root"
cargo test -p secure-store "$@" --test store_roundtrip -- --ignored --exact os_store_roundtrip --nocapture
