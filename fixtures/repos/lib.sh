#!/usr/bin/env bash
# Shared helpers for deterministic fixture repos. Source this file; do not execute it.
# Determinism contract: fixed UUID, fixed author, fixed per-revision dates, seeded binary data,
# so `svnadmin dump` output (and its SHA-256) is identical on every run and svn version.

# svn needs a UTF-8 locale for the Vietnamese path names.
export LC_ALL=C.UTF-8
FIXTURE_AUTHOR="fixture"

# Create an empty repo with a fixed UUID. Usage: fx_create <repo-path> <uuid>
fx_create() {
  rm -rf "$1"
  svnadmin create "$1"
  svnadmin setuuid "$1" "$2"
}

# Deterministic pseudo-random bytes. Usage: fx_random_bytes <seed-label> <count> > file
# AES-CTR keystream over zeros with a key derived from the label: stable across openssl builds.
fx_random_bytes() {
  local key
  key=$(printf '%s' "$1" | openssl dgst -sha256 -r | cut -c1-32)
  head -c "$2" /dev/zero | openssl enc -aes-128-ctr -K "$key" -iv 00000000000000000000000000000000
}

# Rewrite svn:date / svn:author of every revision to fixed values (rev N = epoch + N hours).
# Usage: fx_pin_revprops <repo-path>
fx_pin_revprops() {
  local repo="$1" head rev tmp
  head=$(svnlook youngest "$repo")
  # Plain files, not <(...): svnadmin 1.9 cannot read a revprop value from a pipe.
  tmp=$(mktemp)
  for rev in $(seq 0 "$head"); do
    printf '2024-01-01T%02d:00:00.000000Z' "$rev" > "$tmp"
    svnadmin setrevprop "$repo" -r "$rev" svn:date "$tmp"
    if [ "$rev" -gt 0 ]; then
      printf '%s' "$FIXTURE_AUTHOR" > "$tmp"
      svnadmin setrevprop "$repo" -r "$rev" svn:author "$tmp"
    fi
  done
  rm -f "$tmp"
}

# Verify (or with FIXTURE_UPDATE_HASH=1, record) the SHA-256 of the full dump.
# Usage: fx_check_hash <repo-path> <name>
fx_check_hash() {
  local repo="$1" name="$2" actual expected_file series
  expected_file="$(dirname "${BASH_SOURCE[0]}")/expected/${name}.sha256"
  # svnadmin 1.8 writes the same dump with different header order, so it has its own pin.
  series=$(svnadmin --version --quiet | cut -d. -f1-2)
  if [ -f "${expected_file%.sha256}.${series}.sha256" ]; then
    expected_file="${expected_file%.sha256}.${series}.sha256"
  fi
  actual=$(svnadmin dump -q "$repo" | sha256sum | cut -d' ' -f1)
  if [ "${FIXTURE_UPDATE_HASH:-0}" = "1" ]; then
    printf '%s\n' "$actual" > "$expected_file"
    echo "recorded $name sha256=$actual"
    return 0
  fi
  if [ ! -f "$expected_file" ]; then
    echo "no expected hash for $name (run with FIXTURE_UPDATE_HASH=1); actual=$actual" >&2
    return 1
  fi
  if [ "$(cat "$expected_file")" != "$actual" ]; then
    echo "HASH MISMATCH for $name: expected $(cat "$expected_file") actual $actual" >&2
    return 1
  fi
  echo "$name ok sha256=$actual"
}

# Commit with fixed author, then update so the next property change on a directory is not
# rejected as out of date. Usage: fx_commit <wc> <message>
fx_commit() {
  svn commit --username "$FIXTURE_AUTHOR" --non-interactive -m "$2" "$1" >/dev/null
  svn update -q "$1"
}
