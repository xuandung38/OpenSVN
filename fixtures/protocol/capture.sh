#!/usr/bin/env bash
# Records whole svn <-> mod_dav_svn sessions (HTTPv2) for checkout, update, log and commit and
# writes them, anonymised, to fixtures/protocol/<session>/NNN-{request,response}.http.
#
# Usage: fixtures/protocol/capture.sh [session...]      (default: all sessions)
# Needs (native mode): svn, svnadmin, apache2 (httpd 2.4) with mod_dav_svn, htpasswd, python3,
# openssl. On Debian/Ubuntu: apt-get install subversion apache2 libapache2-mod-svn.
# The server repository is fixtures/repos/make-basic.sh (hash-pinned), served at /svn/basic with
# Basic auth; requests go through crates/svn-protocol/tools/capture/proxy.py, which logs every
# exchange with identity encoding (no gzip) so bodies are readable.
#
# Determinism: the repository content, revision dates and authors are pinned, so every response
# except commit-time data is stable. normalize.py rewrites host/port to svn.test.local, drops
# Authorization/Cookie headers, pins Date/Last-Modified/Server/Keep-Alive headers and replaces
# timestamps that are not pinned repository dates (the new commit's svn:date) with a fixed value.
set -euo pipefail
export LC_ALL=C.UTF-8 TZ=UTC
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
proxy_py="$root/crates/svn-protocol/tools/capture/proxy.py"
SESSIONS=("$@")
[ "${#SESSIONS[@]}" -gt 0 ] || SESSIONS=(checkout checkout-skelta update log commit)

APACHE_PORT="${CAPTURE_APACHE_PORT:-18090}"
PROXY_PORT="${CAPTURE_PROXY_PORT:-18091}"
MODS=/usr/lib/apache2/modules
work="$(mktemp -d)"; chmod 755 "$work"
cleanup() {
  [ -n "${PROXY:-}" ] && kill "$PROXY" 2>/dev/null || true
  [ -f "$work/httpd.pid" ] && kill "$(cat "$work/httpd.pid")" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

# --- server ------------------------------------------------------------------------------------
mkdir -p "$work/repos" "$work/logs"
bash "$root/fixtures/repos/make-basic.sh" "$work/repos/basic" >/dev/null
htpasswd -bc "$work/htpasswd" tester tester-pass 2>/dev/null
printf '#!/bin/sh\nexit 0\n' > "$work/repos/basic/hooks/pre-revprop-change"
chmod +x "$work/repos/basic/hooks/pre-revprop-change"
run_user=www-data
if [ "$(id -u)" != 0 ]; then run_user="$(id -un)"; fi
[ "$(id -u)" = 0 ] && chown -R "$run_user" "$work/repos"
cat > "$work/httpd.conf" <<EOF
ServerRoot "$work"
ServerName svn.test.local
Listen 127.0.0.1:$APACHE_PORT
PidFile "$work/httpd.pid"
ErrorLog "$work/logs/error.log"
LoadModule mpm_event_module $MODS/mod_mpm_event.so
LoadModule authn_core_module $MODS/mod_authn_core.so
LoadModule authz_core_module $MODS/mod_authz_core.so
LoadModule authn_file_module $MODS/mod_authn_file.so
LoadModule authz_user_module $MODS/mod_authz_user.so
LoadModule auth_basic_module $MODS/mod_auth_basic.so
LoadModule dav_module $MODS/mod_dav.so
LoadModule dav_svn_module $MODS/mod_dav_svn.so
$( [ "$(id -u)" = 0 ] && printf 'LoadModule unixd_module %s/mod_unixd.so\nUser %s\nGroup %s\n' "$MODS" "$run_user" "$run_user" )
<Location /svn>
  DAV svn
  SVNParentPath "$work/repos"
  SVNAdvertiseV2Protocol On
  SVNAutoversioning off
  SVNAllowBulkUpdates Prefer
  AuthType Basic
  AuthName "OpenSVN Test"
  AuthUserFile "$work/htpasswd"
  Require valid-user
</Location>
EOF
# mod_unixd is compiled in on some builds; drop the LoadModule line if it is.
if apache2 -l 2>/dev/null | grep -q mod_unixd.c; then sed -i '/unixd_module/d' "$work/httpd.conf"; fi
apache2 -f "$work/httpd.conf" -k start
python3 "$proxy_py" "$PROXY_PORT" "$APACHE_PORT" "$work/cur.jsonl" & PROXY=$!
for _ in $(seq 50); do curl -s -o /dev/null "http://127.0.0.1:$PROXY_PORT/" && break; sleep 0.1; done

server_version="$(apache2 -v | head -1 | sed 's/^Server version: //') / $(svnadmin --version --quiet)"
client_version="$(svn --version --quiet)"
U="http://127.0.0.1:$PROXY_PORT/svn/basic"
A=(--username tester --password tester-pass --non-interactive --no-auth-cache
   --config-option servers:global:http-compression=no)

record() { # record <session> <description> <command...>
  local name="$1" desc="$2"
  shift 2
  : > "$work/cur.jsonl"
  "$@" > "$work/$name.out" 2>&1 || { echo "$name: command failed" >&2; cat "$work/$name.out" >&2; exit 1; }
  rm -rf "${here:?}/$name"
  python3 "$here/normalize.py" "$work/cur.jsonl" "$here/$name" "$PROXY_PORT" \
    "$desc" "$client_version" "$server_version" "$*"
}

for s in "${SESSIONS[@]}"; do
  case "$s" in
    checkout)
      record checkout "svn checkout of trunk@HEAD (bulk update-report, send-all)" \
        svn "${A[@]}" checkout "$U/trunk" "$work/wc-co" ;;
    checkout-skelta)
      record checkout-skelta "svn checkout of trunk@HEAD with http-bulk-updates=no (skelta + GETs)" \
        svn "${A[@]}" --config-option servers:global:http-bulk-updates=no checkout "$U/trunk" "$work/wc-sk" ;;
    update)
      svn "${A[@]}" checkout -q -r 4 "$U/trunk" "$work/wc-up"
      record update "svn update of a trunk WC from r4 to HEAD (rename, copy, props, externals)" \
        svn "${A[@]}" update "$work/wc-up" ;;
    log)
      record log "svn log --xml -v --with-all-revprops -l 5 on trunk, then log of README.txt" \
        sh -c 'svn "$@" log --xml -v --with-all-revprops -l 5 '"$U/trunk"' && svn "$@" log '"$U/trunk/README.txt" _ "${A[@]}" ;;
    commit)
      w="$work/wc-ci"
      svn "${A[@]}" checkout -q "$U/trunk" "$w"
      printf 'committed line\n' >> "$w/README.txt"
      printf 'brand new\n' > "$w/added.txt"
      svn add -q "$w/added.txt"
      svn propset -q custom:capture yes "$w/src/app.c"
      svn rm -q "$w/crlf.txt"
      record commit "svn commit: modify README.txt, add added.txt, propset on src/app.c, delete crlf.txt" \
        svn "${A[@]}" commit -m "Capture commit" "$w" ;;
    *) echo "unknown session $s" >&2; exit 2 ;;
  esac
  echo "  $s ok"
done
