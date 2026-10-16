#!/usr/bin/env bash
# Working copies created by the engine, used by an older svn client (#20, #21): the engine
# (crash-worker: svn-ra + svn-client + svn-wc, as the app wires them) checks out, commits and
# updates; then the old client runs status, commit and update on the same working copy, and the
# engine updates it again afterwards. Every step compares `svn status` and the file contents.
#
#   scripts/test-env/engine-wc-old-client.sh <crash-worker> [old-svn-command...]
#
# The old client defaults to the compat image of svn 1.14.5 (`opensvn-compat/svn-client:1.14.5`,
# built by `cargo xtask compat up`), run in Docker as the calling user on the host network; pass
# another command (e.g. /usr/bin/svn) to use a local binary. The server is a throwaway Apache +
# mod_dav_svn on 127.0.0.1 (anonymous, plain HTTP: the engine path, not authentication, is under
# test), like the crash suite's. Needs apache2, libapache2-mod-svn and svnadmin on the host.
set -euo pipefail

worker="${1:?usage: engine-wc-old-client.sh <crash-worker> [old-svn-command...]}"
shift
work="$(mktemp -d "${TMPDIR:-/tmp}/opensvn-oldclient-XXXXXX")"
chmod 755 "$work"
if [ "$#" -gt 0 ]; then
  old=("$@")
else
  old=(docker run --rm --network host --user "$(id -u):$(id -g)" -e HOME=/tmp
    -v "$work:$work" opensvn-compat/svn-client:1.14.5 svn)
fi

httpd_pid=""
cleanup() {
  [ -n "$httpd_pid" ] && kill "$httpd_pid" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT
fail() { echo "engine-wc-old-client: $*" >&2; exit 1; }

# --- server -------------------------------------------------------------------------------------
modules=/usr/lib/apache2/modules
[ -f "$modules/mod_dav_svn.so" ] || fail "mod_dav_svn not found in $modules (apt install libapache2-mod-svn)"
port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
mkdir -p "$work/repos"
{
  for m in mpm_event unixd authz_core dav dav_svn; do
    [ -f "$modules/mod_$m.so" ] && echo "LoadModule ${m}_module $modules/mod_$m.so"
  done
  cat <<EOF
ServerRoot $work
ServerName 127.0.0.1
Listen 127.0.0.1:$port
PidFile $work/httpd.pid
ErrorLog $work/error.log
<Location /svn>
  DAV svn
  SVNParentPath $work/repos
</Location>
EOF
  if [ "$(id -u)" = 0 ]; then echo "User www-data"; echo "Group www-data"; fi
} >"$work/httpd.conf"

svnadmin create "$work/repos/r"
mkdir -p "$work/import/trunk/src" "$work/import/trunk/docs" "$work/import/branches" "$work/import/tags"
for i in $(seq 1 40); do printf 'line one %s\nline two\nline three\n' "$i" >"$work/import/trunk/src/f$i.txt"; done
printf 'readme\n' >"$work/import/trunk/README"
svn import -q -m "Initial import" "$work/import" "file://$work/repos/r"
[ "$(id -u)" = 0 ] && chown -R www-data:www-data "$work/repos"

/usr/sbin/apache2 -f "$work/httpd.conf" -DFOREGROUND &
httpd_pid=$!
for _ in $(seq 1 100); do
  (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null && break
  sleep 0.1
done
# Checkout and update open their session at the working copy's URL, commit at the repository root
# (as crash-worker does for the crash suite).
root="http://127.0.0.1:$port/svn/r"
url="$root/trunk"
wc="$work/wc"
state="$work/state"

engine() {
  local out
  out="$("$worker" "$state" "$@" 2>&1)" || fail "engine $1 failed: $out"
  echo "engine $1: $(tail -n1 <<<"$out")"
}
old_svn() { "${old[@]}" --non-interactive "$@"; }
# `svn status` of the old client, with the paths relative to the working copy.
old_status() { old_svn status "$wc" | sed "s|$wc/||"; }
expect_status() {
  local got
  got="$(old_status)"
  [ "$got" = "$1" ] || fail "$2: old client status is
$got
expected
$1"
}

echo "old client: $(old_svn --version --quiet)"

# --- engine creates and changes the working copy ------------------------------------------------
engine checkout "$url" "$wc"
printf 'line one 1\nline two (engine)\nline three\n' >"$wc/src/f1.txt"
printf 'line one 2\nline two\nline three (engine)\n' >"$wc/src/f2.txt"
engine commit "$root" "$wc" "Engine commit"
engine update "$url" "$wc"

# --- the old client reads and writes it ---------------------------------------------------------
expect_status "" "after the engine's checkout, commit and update"
[ "$(old_svn info --show-item revision "$wc")" = 2 ] || fail "old client: working copy not at r2"
old_svn log -q -r 2 "$wc" | grep -q "^r2 " || fail "old client: r2 missing from the log"

printf 'line one 3\nline two (old client)\nline three\n' >"$wc/src/f3.txt"
expect_status "M       src/f3.txt" "after a local edit"
old_svn commit -q -m "Old client commit" "$wc"
old_svn update -q "$wc"
expect_status "" "after the old client's commit and update"
[ "$(old_svn info --show-item revision "$wc")" = 3 ] || fail "old client: working copy not at r3"

# Someone else commits; the old client updates the engine's working copy.
svn checkout -q "file://$work/repos/r/trunk" "$work/other"
printf 'line one 4\nline two\nline three (other)\n' >"$work/other/src/f4.txt"
svn commit -q -m "Other commit" "$work/other"
[ "$(id -u)" = 0 ] && chown -R www-data:www-data "$work/repos"
printf 'line one 4 (local)\nline two\nline three\n' >"$wc/src/f4.txt"
old_svn update -q "$wc"
expect_status "M       src/f4.txt" "after the old client merged r4 into a local edit"
[ "$(cat "$wc/src/f4.txt")" = "$(printf 'line one 4 (local)\nline two\nline three (other)')" ] ||
  fail "old client: the merge of r4 lost a change"

# --- the engine takes the working copy back -----------------------------------------------------
engine status "$url" "$wc"
engine commit "$root" "$wc" "Engine commit after the old client"
engine update "$url" "$wc"
expect_status "" "after the engine committed the old client's merge"
[ "$(old_svn info --show-item revision "$wc")" = 5 ] || fail "old client: working copy not at r5"
[ "$(svn cat "file://$work/repos/r/trunk/src/f4.txt")" = "$(cat "$wc/src/f4.txt")" ] ||
  fail "r5 does not hold the merged file"
echo "engine-wc-old-client: ok (engine and $(old_svn --version --quiet) share the working copy)"
