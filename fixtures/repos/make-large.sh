#!/usr/bin/env bash
# Perf fixture: a repo with N files (default 100000) in 1000-file directories.
# Generated on demand, never committed; content is deterministic but no hash is pinned
# (generation time varies and this repo is only used for timing).
# Usage: make-large.sh <repo-path> [file-count]
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
. "$here/lib.sh"

repo="${1:?usage: make-large.sh <repo-path> [file-count]}"
count="${2:-100000}"
fx_create "$repo" "6d9f2c1e-0b1a-4c5e-9a31-0a5f2e1d7b03"
src="$(mktemp -d)"
trap 'rm -rf "$src"' EXIT
mkdir -p "$src/trunk"
awk -v n="$count" -v root="$src/trunk" 'BEGIN {
  for (i = 0; i < n; i++) {
    dir = sprintf("%s/d%04d", root, int(i / 1000));
    if (i % 1000 == 0) system("mkdir -p " dir);
    f = sprintf("%s/f%06d.txt", dir, i);
    printf "file %d\nline two\n", i > f;
    close(f);
  }
}'
svn import -q --username "$FIXTURE_AUTHOR" -m "Import $count files" "$src" "file://$repo"
echo "large repo ready: $count files at $repo"
