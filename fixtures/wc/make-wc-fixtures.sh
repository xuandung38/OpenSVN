#!/usr/bin/env bash
# Working-copy state fixtures: one tarball per state, produced by a real svn client.
#
# Usage: fixtures/wc/make-wc-fixtures.sh <format> [state...]
#   <format>  31 -> default WC format of svn 1.14.x (output: fixtures/wc/f31/) or of
#                   svn 1.15.x (output: fixtures/wc/f31-svn1.15/; both sets are kept)
#             32 -> svn 1.15.x `--compatible-version 1.15`     (output: fixtures/wc/f32/)
#   state     optional subset (default: every state of that format)
# Env: SVN       client binary for the WC (default: svn from PATH)
#      SVNSERVE  svnserve next to $SVN (default), used through an svn+ssh tunnel for the
#                incomplete and server-excluded states only
#      OUT       output directory (default as above)
#      The fixture repo is always built with the svnadmin/svn on PATH (dump hash is pinned).
# Needs: bash, openssl, sha256sum, tar (GNU), zstd, python3 (manifest only).
# svn 1.15.0 for the f31-svn1.15 and f32 sets: `nix-build docker/nix/subversion-1.15.nix`.
#
# Reproducibility: the repository and every state are deterministic (pinned UUID, dates, authors,
# seeded data), and `<state>.status.txt` (`svn status -v` + `svn info` subset) is byte-identical
# across runs. The tarball bytes are not: wc.db stores local mtimes (`NODES.last_mod_time`) and
# lock creation times, and SQLite page layout depends on insertion timing. Compare via the
# status files / the compat harness wc.db dump, not the tarball hash.
#
# Paths: everything lives under the fixed root /tmp/opensvn-wc-fixtures so the repository URL
# recorded in each wc.db (file:///tmp/opensvn-wc-fixtures/repos/basic) is the same on every run.
# Consumers that need the repository can rebuild it with this script (`repo` state) or relocate.
set -euo pipefail
export LC_ALL=C.UTF-8 TZ=UTC
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fmt="${1:?usage: make-wc-fixtures.sh <31|32> [state...]}"
shift
SVN="${SVN:-svn}"
ROOT=/tmp/opensvn-wc-fixtures
REPO="$ROOT/repos/basic"
URL="file://$REPO"
U=(--username fixture --non-interactive)

case "$fmt" in
  31) CO_OPTS=() ;;
  32) CO_OPTS=(--compatible-version 1.15) ;;
  *) echo "format must be 31 or 32" >&2; exit 2 ;;
esac

ver="$("$SVN" --version --quiet)"
if [ -z "${OUT:-}" ]; then
  case "$fmt:$ver" in
    31:1.15.*) OUT="$here/f31-svn1.15" ;;
    *) OUT="$here/f$fmt" ;;
  esac
fi
SVNSERVE="${SVNSERVE:-$(dirname "$(command -v "$SVN")")/svnserve}"
echo "svn client $ver, format $fmt -> $OUT"

# ---------------------------------------------------------------------------------------------
# Repository: fixtures/repos/make-basic.sh (r1..r10, hash-pinned) plus r11..r14 used by the
# conflict states. Rebuilt from scratch for every run so all states see the same history.
build_repo() {
  rm -rf "$ROOT"
  mkdir -p "$ROOT/repos" "$ROOT/work"
  bash "$here/../repos/make-basic.sh" "$REPO" >/dev/null
  local w="$ROOT/work/prep"
  svn checkout -q "$URL/trunk" "$w"
  # r11: incoming text change on README.txt line 2 (text conflict)
  printf 'Hello OpenSVN\nsecond line changed upstream\nthird line\n' > "$w/README.txt"
  svn commit -q "${U[@]}" -m "Upstream README change" "$w"
  # r12: incoming property change (property conflict)
  svn propset -q custom:reviewed upstream "$w/src/app.c"
  svn commit -q "${U[@]}" -m "Upstream property change" "$w"
  # r13: incoming delete of docs/index.md (tree conflict: local edit, incoming delete)
  svn rm -q "$w/docs/index.md"
  svn commit -q "${U[@]}" -m "Upstream delete of docs/index.md" "$w"
  # r14: incoming add of new.txt (obstruction by an unversioned local file)
  printf 'added upstream\n' > "$w/new.txt"
  svn add -q "$w/new.txt"
  svn commit -q "${U[@]}" -m "Upstream add of new.txt" "$w"
  rm -rf "$w"
  # Same pinned revprops as lib.sh, applied to the extra revisions as well.
  # shellcheck source=../repos/lib.sh
  . "$here/../repos/lib.sh"
  fx_pin_revprops "$REPO"
}

co() { # co <wc> [rev] [extra checkout args...]
  local wc="$1" rev="${2:-HEAD}"
  shift 2 || shift $#
  "$SVN" checkout -q "${CO_OPTS[@]}" -r "$rev" "$@" "$URL/trunk" "$wc"
}

# ---------------------------------------------------------------------------------------------
# States. Each function receives an empty directory path, creates the WC there and leaves it in
# the described state. Descriptions go into manifest.json.
declare -A DESC
s() { DESC[$1]="$2"; }

s clean "pristine checkout of trunk@HEAD (keywords, eol-style, needs-lock, externals expanded)"
st_clean() { co "$1"; }

s modified-text "README.txt and crlf.txt edited"
st_modified-text() { co "$1"; printf 'local edit\n' >> "$1/README.txt"; printf 'x\r\n' >> "$1/crlf.txt"; }

s modified-binary "data/blob.bin overwritten with different bytes"
st_modified-binary() { co "$1"; head -c 70000 /dev/zero | tr '\0' 'b' > "$1/data/blob.bin"; }

s modified-props "property added, changed and deleted on file and directory"
st_modified-props() {
  co "$1"
  "$SVN" propset -q custom:local yes "$1/README.txt"
  "$SVN" propset -q svn:keywords "Id Rev Author" "$1/keywords.txt"
  "$SVN" propdel -q svn:executable "$1/run.sh"
  "$SVN" propset -q svn:ignore '*.log' "$1/src"
}

s added "new file, new directory tree and an added binary"
st_added() {
  co "$1"
  printf 'new\n' > "$1/added.txt"
  mkdir -p "$1/newdir/sub"; printf 'deep\n' > "$1/newdir/sub/deep.txt"
  head -c 1024 /dev/zero > "$1/added.bin"
  "$SVN" add -q "$1/added.txt" "$1/newdir" "$1/added.bin"
}

s deleted "file and directory scheduled for deletion"
st_deleted() { co "$1"; "$SVN" rm -q "$1/crlf.txt" "$1/docs"; }

s missing "versioned file and directory removed without svn (status !)"
st_missing() { co "$1"; rm -f "$1/mixed-eol.txt"; rm -rf "$1/src"; }

s unversioned-ignored "unversioned files, one matching svn:ignore (*.o, build)"
st_unversioned-ignored() {
  co "$1"
  printf 'u\n' > "$1/unversioned.txt"; printf 'o\n' > "$1/obj.o"; mkdir "$1/build"
  printf 'b\n' > "$1/build/out.bin"
}

s replaced "file deleted and re-added in the same place (status R)"
st_replaced() { co "$1"; "$SVN" rm -q "$1/README.txt"; printf 'replacement\n' > "$1/README.txt"; "$SVN" add -q "$1/README.txt"; }

s copied "uncommitted copy of a file and a directory (copyfrom recorded)"
st_copied() { co "$1"; "$SVN" cp -q "$1/src/app.c" "$1/src/app2.c"; "$SVN" cp -q "$1/docs" "$1/docs-copy"; }

s moved-file "uncommitted move of a file (moved-from / moved-to)"
st_moved-file() { co "$1"; "$SVN" mv -q "$1/README.txt" "$1/README.md"; }

s moved-dir "uncommitted move of a directory, then an edit inside the moved copy"
st_moved-dir() { co "$1"; "$SVN" mv -q "$1/src" "$1/source"; printf '// x\n' >> "$1/source/app.c"; }

s conflict-text "text conflict on README.txt (checkout r10, local edit, update to r11)"
st_conflict-text() {
  co "$1" 10
  printf 'Hello OpenSVN\nsecond line changed locally\nthird line\n' > "$1/README.txt"
  "$SVN" update -q -r 11 --accept postpone "$1" >/dev/null
}

s conflict-prop "property conflict on src/app.c (local vs upstream custom:reviewed)"
st_conflict-prop() {
  co "$1" 11
  "$SVN" propset -q custom:reviewed locally "$1/src/app.c"
  "$SVN" update -q -r 12 --accept postpone "$1" >/dev/null
}

s conflict-tree "tree conflict: local edit of docs/index.md, incoming delete (r13)"
st_conflict-tree() {
  co "$1" 12
  printf 'local doc edit\n' >> "$1/docs/index.md"
  "$SVN" update -q -r 13 --accept postpone "$1" >/dev/null
}

s obstructed "unversioned local new.txt obstructs the incoming add (update --force not used)"
st_obstructed() {
  co "$1" 13
  printf 'local, unversioned\n' > "$1/new.txt"
  "$SVN" update -q -r 14 --accept postpone "$1" >/dev/null 2>&1 || true
}

s kind-changed "versioned file replaced on disk by a directory (status ~)"
st_kind-changed() { co "$1"; rm "$1/run.sh"; mkdir "$1/run.sh"; }

s locked "locked.txt locked by this WC (lock token in wc.db, file writable)"
st_locked() { co "$1"; "$SVN" lock -q "${U[@]}" -m "fixture lock" "$1/locked.txt"; }

s sparse-empty "checkout --depth empty"
st_sparse-empty() { co "$1" HEAD --depth empty; }

s sparse-files "checkout --depth files"
st_sparse-files() { co "$1" HEAD --depth files; }

s sparse-immediates "checkout --depth immediates, then src deepened to infinity"
st_sparse-immediates() { co "$1" HEAD --depth immediates; "$SVN" update -q --set-depth infinity "$1/src"; }

s excluded "docs excluded with --set-depth exclude"
st_excluded() { co "$1"; "$SVN" update -q --set-depth exclude "$1/docs"; }

s externals "dir external ^/trunk/docs and file external ^/trunk/README.txt; the file external edited"
# clean also contains the externals; this state additionally edits inside the dir external.
st_externals() { co "$1"; printf 'ext edit\n' >> "$1/with-externals/readme-ext.txt"; }

s mixed-rev "mixed revisions: WC at r14, README.txt at r10, docs at r12"
st_mixed-rev() { co "$1"; "$SVN" update -q -r 10 "$1/README.txt"; "$SVN" update -q -r 12 "$1/docs"; }

s switched "src switched to branches/feature-x/src"
st_switched() { co "$1"; "$SVN" switch -q "$URL/branches/feature-x/src" "$1/src"; }

s changelist "two files assigned to changelist 'fixture-cl', one of them edited"
st_changelist() {
  co "$1"
  printf 'cl\n' >> "$1/README.txt"
  "$SVN" changelist -q fixture-cl "$1/README.txt" "$1/src/app.c"
}

s merged-uncommitted "branch change merged into trunk WC, not committed (svn:mergeinfo modified)"
st_merged-uncommitted() { co "$1"; "$SVN" merge -q "$URL/branches/feature-x" "$1" >/dev/null; }

s unicode-paths "added files with Vietnamese (NFC) names and spaces"
st_unicode-paths() {
  co "$1"
  mkdir "$1/thư mục"; printf 'xin chào\n' > "$1/thư mục/tệp tin.txt"
  "$SVN" add -q "$1/thư mục"
}

s no-pristine "format 32 only: checkout --store-pristine no, README.txt edited (pristine not stored)"
st_no-pristine() { co "$1" HEAD --store-pristine no; printf 'edit\n' >> "$1/README.txt"; }

# --- States added with the svn 1.15.0 sets. None of them adds a repository revision, so the
# --- repository (and the committed 1.14.3 f31 set) stays exactly as before.

s conflict-tree-local-delete "tree conflict: README.txt deleted locally, incoming edit (r11)"
st_conflict-tree-local-delete() {
  co "$1" 10
  "$SVN" rm -q "$1/README.txt"
  "$SVN" update -q -r 11 --accept postpone "$1" >/dev/null
}

s conflict-tree-moved-away "tree conflict: README.txt moved away locally, incoming edit (r11)"
st_conflict-tree-moved-away() {
  co "$1" 10
  "$SVN" mv -q "$1/README.txt" "$1/README-moved.txt"
  "$SVN" update -q -r 11 --accept postpone "$1" >/dev/null
}

s conflict-tree-add "tree conflict: new.txt added locally (versioned), incoming add (r14)"
st_conflict-tree-add() {
  co "$1" 13
  printf 'added locally\n' > "$1/new.txt"
  "$SVN" add -q "$1/new.txt"
  "$SVN" update -q -r 14 --accept postpone "$1" >/dev/null
}

s conflict-tree-delete-delete "tree conflict: docs/index.md deleted locally, incoming delete (r13)"
st_conflict-tree-delete-delete() {
  co "$1" 12
  "$SVN" rm -q "$1/docs/index.md"
  "$SVN" update -q -r 13 --accept postpone "$1" >/dev/null
}

s conflict-merge-text "text conflict from a merge: README.txt edited, reverse merge of r11"
st_conflict-merge-text() {
  co "$1"
  printf 'Hello OpenSVN\nsecond line changed locally\nthird line\n' > "$1/README.txt"
  "$SVN" merge -q -c -11 --accept postpone "$URL/trunk" "$1" >/dev/null
}

s conflict-merge-tree "tree conflict from a merge: README.txt deleted locally, reverse merge of r11"
st_conflict-merge-tree() {
  co "$1"
  "$SVN" rm -q "$1/README.txt"
  "$SVN" merge -q -c -11 --accept postpone "$URL/trunk" "$1" >/dev/null
}

s conflict-binary "binary conflict: data/blob.bin edited, two-URL merge brings other bytes"
st_conflict-binary() {
  co "$1"
  head -c 512 /dev/zero | tr '\0' 'L' > "$1/data/blob.bin"
  "$SVN" merge -q --accept postpone --ignore-ancestry \
    "$URL/trunk/data/blob.bin@2" "$URL/trunk/data/image.png@2" "$1/data/blob.bin" >/dev/null
}

s obstructed-dir "unversioned local directory new.txt obstructs the incoming file add (r14)"
st_obstructed-dir() {
  co "$1" 13
  mkdir "$1/new.txt"; printf 'inside\n' > "$1/new.txt/keep.txt"
  "$SVN" update -q -r 14 --accept postpone "$1" >/dev/null 2>&1 || true
}

s replaced-dir "directory docs deleted and re-added with new content (status R on a directory)"
st_replaced-dir() {
  co "$1"
  "$SVN" rm -q "$1/docs"
  mkdir "$1/docs"; printf 'new docs\n' > "$1/docs/new.md"
  "$SVN" add -q "$1/docs"
}

s switched-file "README.txt switched to branches/feature-x/README.txt"
st_switched-file() { co "$1"; "$SVN" switch -q "$URL/branches/feature-x/README.txt" "$1/README.txt"; }

s sparse-mixed "depth infinity root with src set to empty and docs set to files"
st_sparse-mixed() {
  co "$1"
  "$SVN" update -q --set-depth empty "$1/src"
  "$SVN" update -q --set-depth files "$1/docs"
}

s lock-stolen "locked.txt locked by this WC, then the lock was stolen elsewhere (stale token)"
st_lock-stolen() {
  co "$1"
  "$SVN" lock -q "${U[@]}" -m "fixture lock" "$1/locked.txt"
  "$SVN" lock -q --username thief --non-interactive --force -m "stolen" "$URL/trunk/locked.txt"
}

# svn+ssh tunnel to svnserve, used only to make the server produce what file:// cannot: an
# authz-hidden subtree and a connection cut mid-update. Host name is a .test placeholder; the
# tunnel script ignores it. The WC is relocated back to file:// afterwards.
tunnel() { # tunnel [byte limit]  -> prints the SVN_SSH command
  local cfg="$ROOT/svnserve" limit="${1:-}"
  mkdir -p "$cfg"
  printf '[general]\nanon-access = read\nauth-access = write\nauthz-db = %s\n' "$cfg/authz" \
    > "$cfg/svnserve.conf"
  printf '[/]\n* = rw\n\n[/trunk/data]\n* =\n' > "$cfg/authz"
  printf '[general]\nanon-access = read\nauth-access = write\n' > "$cfg/plain.conf"
  # Forwards svnserve's answers until `limit` bytes, then kills it and closes the connection.
  cat > "$cfg/cut.py" <<'PYEOF'
import os, subprocess, sys, threading
limit = int(sys.argv[1])
p = subprocess.Popen(sys.argv[2:], stdin=subprocess.PIPE, stdout=subprocess.PIPE, bufsize=0)
def pump_in():
    try:
        while (b := os.read(0, 65536)):
            p.stdin.write(b)
    except OSError:
        pass
threading.Thread(target=pump_in, daemon=True).start()
sent = 0
while sent < limit and (b := os.read(p.stdout.fileno(), min(65536, limit - sent))):
    os.write(1, b)
    sent += len(b)
p.kill()
os._exit(0)
PYEOF
  local script="$ROOT/tunnel-${limit:-full}.sh"
  if [ -n "$limit" ]; then
    printf '#!/bin/sh\nexec python3 "%s" %s "%s" -t --tunnel-user fixture --config-file "%s"\n' \
      "$cfg/cut.py" "$limit" "$SVNSERVE" "$cfg/plain.conf" > "$script"
  else
    printf '#!/bin/sh\nexec "%s" -t --tunnel-user fixture --config-file "%s"\n' \
      "$SVNSERVE" "$cfg/svnserve.conf" > "$script"
  fi
  chmod +x "$script"
  echo "$script"
}
SSH_URL="svn+ssh://fixtures.test$REPO"

s server-excluded "trunk/data hidden by server authz (presence server-excluded), via svnserve"
st_server-excluded() {
  local t; t="$(tunnel)"
  SVN_SSH="$t" "$SVN" checkout -q "${CO_OPTS[@]}" "$SSH_URL/trunk" "$1"
  SVN_SSH="$t" "$SVN" relocate "$SSH_URL" "$URL" "$1"
}

s incomplete "update r9 -> r14 cut off mid-stream by the server (presence incomplete)"
st_incomplete() {
  co "$1" 9
  local full cut
  full="$(tunnel)"; cut="$(tunnel 3000)"
  SVN_SSH="$full" "$SVN" relocate "$URL" "$SSH_URL" "$1"
  if SVN_SSH="$cut" "$SVN" update -q -r 14 "$1" >/dev/null 2>&1; then
    echo "incomplete: the update was not interrupted" >&2; exit 1
  fi
  "$SVN" relocate "$SSH_URL" "$URL" "$1"
}

NEW_115=(conflict-tree-local-delete conflict-tree-moved-away conflict-tree-add
  conflict-tree-delete-delete conflict-merge-text conflict-merge-tree conflict-binary obstructed-dir
  replaced-dir switched-file sparse-mixed lock-stolen server-excluded incomplete)
ALL31=(clean modified-text modified-binary modified-props added deleted missing unversioned-ignored
  replaced copied moved-file moved-dir conflict-text conflict-prop conflict-tree obstructed
  kind-changed locked sparse-empty sparse-files sparse-immediates excluded externals mixed-rev
  switched changelist merged-uncommitted unicode-paths)
# The 1.14.3 f31 set was made before NEW_115 existed; svn 1.15 runs add them.
case "$ver" in 1.15.*) ALL31+=("${NEW_115[@]}") ;; esac
ALL32=("${ALL31[@]}" no-pristine)

if [ "$#" -gt 0 ]; then STATES=("$@"); elif [ "$fmt" = 31 ]; then STATES=("${ALL31[@]}"); else STATES=("${ALL32[@]}"); fi

# ---------------------------------------------------------------------------------------------
build_repo
mkdir -p "$OUT"
rows="$ROOT/manifest-rows.tsv"; : > "$rows"
for state in "${STATES[@]}"; do
  [ -n "${DESC[$state]:-}" ] || { echo "unknown state $state" >&2; exit 2; }
  dir="$ROOT/work/$state"
  rm -rf "$dir"; mkdir -p "$dir"
  "st_$state" "$dir/wc"
  got_fmt="$(python3 - "$dir/wc/.svn/wc.db" <<'PY'
import sqlite3, sys
print(sqlite3.connect(sys.argv[1]).execute("PRAGMA user_version").fetchone()[0])
PY
)"
  if [ "$got_fmt" != "$fmt" ]; then
    echo "$state: wc.db format $got_fmt, expected $fmt" >&2; exit 1
  fi
  # Status snapshot (relative paths, deterministic).
  {
    echo "# svn status -v"
    (cd "$dir/wc" && "$SVN" status -v --non-interactive)
    echo "# svn info (subset)"
    (cd "$dir/wc" && "$SVN" info --show-item kind . && "$SVN" info --show-item revision . \
      && "$SVN" info --show-item depth . && "$SVN" info --show-item relative-url .)
  } > "$OUT/$state.status.txt"
  tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner --format=gnu \
    -C "$dir" -cf - wc | zstd -q -19 -o "$OUT/$state.tar.zst" -f
  sum="$(sha256sum "$OUT/$state.tar.zst" | cut -d' ' -f1)"
  printf '%s\t%s\t%s\n' "$state" "$sum" "${DESC[$state]}" >> "$rows"
  # Release the repository lock so later states/runs start unlocked.
  case "$state" in
    locked) "$SVN" unlock -q "${U[@]}" --force "$dir/wc/locked.txt" ;;
    lock-stolen) "$SVN" unlock -q "${U[@]}" --force "$URL/trunk/locked.txt" ;;
  esac
  echo "  $state ok"
done

python3 - "$OUT/manifest.json" "$rows" "$fmt" "$ver" "$URL" <<'PY'
import json, sys
out, rows, fmt, ver, url = sys.argv[1:]
states = []
for line in open(rows, encoding="utf-8"):
    name, sha, desc = line.rstrip("\n").split("\t", 2)
    states.append({"name": name, "description": desc, "tarball": f"{name}.tar.zst",
                   "status": f"{name}.status.txt", "sha256": sha})
doc = {"format": int(fmt), "svn_version": ver, "repo_url": url, "wc_url": url + "/trunk",
       "wc_dir_in_tarball": "wc",
       "generator": "fixtures/wc/make-wc-fixtures.sh", "states": states}
with open(out, "w", encoding="utf-8") as f:
    json.dump(doc, f, indent=2, ensure_ascii=False)
    f.write("\n")
PY
echo "wrote $OUT/manifest.json (${#STATES[@]} states)"
