#!/usr/bin/env bash
# VisualSVN-like test server on Linux: MIT Kerberos KDC + Apache 2.4 + mod_auth_gssapi +
# mod_dav_svn (+ mod_authz_svn) over HTTPS with a local CA. See docs/visualsvn-test-env.md.
#
#   scripts/test-env/visualsvn-like.sh install   # apt packages (Debian/Ubuntu, needs root)
#   scripts/test-env/visualsvn-like.sh up        # create realm, CA, repos, start KDC + Apache
#   scripts/test-env/visualsvn-like.sh env       # print the exports the e2e tests need
#   scripts/test-env/visualsvn-like.sh down      # stop KDC + Apache (keeps the state dir)
#   scripts/test-env/visualsvn-like.sh purge     # down + delete the state dir
#
# Everything lives in $VSVN_DIR (default /tmp/opensvn-visualsvn). Test-only secrets; never reuse.
# Runs as root (container/VM) or as a user who may edit /etc/hosts via sudo. When run as root,
# Apache drops to www-data and repositories must be created through $VSVN_DIR/bin/svnadmin.
set -euo pipefail
# Repository names with Vietnamese characters need a UTF-8 locale for svnadmin/svnlook.
export LANG=C.UTF-8 LC_ALL=C.UTF-8

VSVN_DIR="${VSVN_DIR:-/tmp/opensvn-visualsvn}"
REALM="${VSVN_REALM:-OPENSVN.TEST}"
HOST="${VSVN_HOST:-svn.opensvn.test}"
# Short (NetBIOS-style) name of the same server: no service principal exists for it, as for a
# VisualSVN server opened as https://svnhost/ instead of its full name.
SHORT_HOST="${VSVN_SHORT_HOST:-svnhost}"
KDC_PORT="${VSVN_KDC_PORT:-18888}"
HTTPS_PORT="${VSVN_HTTPS_PORT:-18443}"
HTTP_PORT="${VSVN_HTTP_PORT:-18080}"
HTTPS115_PORT="${VSVN_HTTPS115_PORT:-18444}"
# mod_dav_svn 1.15.0 built from source (optional): prefix with libexec/mod_dav_svn.so.
SVN115_PREFIX="${VSVN_SVN115_PREFIX:-/opt/svn115}"
KEEPALIVE_TIMEOUT="${VSVN_KEEPALIVE_TIMEOUT:-5}"
MAX_KEEPALIVE_REQUESTS="${VSVN_MAX_KEEPALIVE_REQUESTS:-100}"
USER_PRINC="alice"
USER_PASS="alice-pass"
BASIC_USER="tester"
BASIC_PASS="tester-pass"

die() { echo "visualsvn-like: $*" >&2; exit 1; }
as_root() { if [ "$(id -u)" = 0 ]; then "$@"; else sudo "$@"; fi; }

apache_user() {
  if [ "$(id -u)" = 0 ]; then echo www-data; else id -un; fi
}

cmd_install() {
  as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    krb5-kdc krb5-admin-server krb5-user libkrb5-dev libclang-dev clang pkg-config \
    apache2 apache2-utils libapache2-mod-auth-gssapi libapache2-mod-svn subversion openssl curl
}

# --- Kerberos ---------------------------------------------------------------------------------

write_krb5_conf() {
  mkdir -p "$VSVN_DIR/krb5kdc"
  cat >"$VSVN_DIR/krb5.conf" <<EOF
[libdefaults]
  default_realm = $REALM
  dns_lookup_realm = false
  dns_lookup_kdc = false
  # The SPN is exactly the URL host (see docs/visualsvn-iwa-checklist.md).
  dns_canonicalize_hostname = false
  rdns = false
  udp_preference_limit = 1
  default_ccache_name = FILE:$VSVN_DIR/ccache

[realms]
  $REALM = {
    kdc = 127.0.0.1:$KDC_PORT
    admin_server = 127.0.0.1:$((KDC_PORT + 1))
  }

[domain_realm]
  .opensvn.test = $REALM
  opensvn.test = $REALM

[logging]
  kdc = FILE:$VSVN_DIR/krb5kdc/kdc.log
EOF
  cat >"$VSVN_DIR/krb5kdc/kdc.conf" <<EOF
[kdcdefaults]
  kdc_ports = $KDC_PORT
  kdc_tcp_ports = $KDC_PORT

[realms]
  $REALM = {
    database_name = $VSVN_DIR/krb5kdc/principal
    acl_file = $VSVN_DIR/krb5kdc/kadm5.acl
    key_stash_file = $VSVN_DIR/krb5kdc/stash
    max_life = 10h 0m 0s
    max_renewable_life = 7d 0h 0m 0s
    supported_enctypes = aes256-cts-hmac-sha1-96:normal aes128-cts-hmac-sha1-96:normal
  }
EOF
  : >"$VSVN_DIR/krb5kdc/kadm5.acl"
}

krb_env() {
  export KRB5_CONFIG="$VSVN_DIR/krb5.conf" KRB5_KDC_PROFILE="$VSVN_DIR/krb5kdc/kdc.conf"
  export KRB5CCNAME="FILE:$VSVN_DIR/ccache"
}

setup_realm() {
  krb_env
  if [ ! -e "$VSVN_DIR/krb5kdc/principal" ]; then
    kdb5_util create -s -r "$REALM" -P "kdc-master-pass" >/dev/null
    kadmin.local -q "addprinc -pw $USER_PASS $USER_PRINC" >/dev/null
    kadmin.local -q "addprinc -pw bob-pass bob" >/dev/null
    kadmin.local -q "addprinc -randkey HTTP/$HOST" >/dev/null
    kadmin.local -q "ktadd -k $VSVN_DIR/http.keytab HTTP/$HOST" >/dev/null
  fi
  chown "$(apache_user)" "$VSVN_DIR/http.keytab"
  chmod 600 "$VSVN_DIR/http.keytab"
}

start_kdc() {
  krb_env
  if [ -s "$VSVN_DIR/kdc.pid" ] && kill -0 "$(cat "$VSVN_DIR/kdc.pid")" 2>/dev/null; then return; fi
  krb5kdc -P "$VSVN_DIR/kdc.pid"
  for _ in $(seq 50); do [ -s "$VSVN_DIR/kdc.pid" ] && break; sleep 0.1; done
}

# --- TLS ----------------------------------------------------------------------------------------

make_certs() {
  local d="$VSVN_DIR/certs"
  [ -s "$d/server.crt" ] && return
  mkdir -p "$d"
  openssl genrsa -out "$d/ca.key" 2048 2>/dev/null
  openssl req -x509 -new -key "$d/ca.key" -sha256 -days 3650 \
    -subj "/CN=OpenSVN VisualSVN-like Test CA" -out "$d/ca.crt"
  openssl genrsa -out "$d/server.key" 2048 2>/dev/null
  openssl req -new -key "$d/server.key" -subj "/CN=$HOST" -out "$d/server.csr"
  printf 'subjectAltName=DNS:%s,DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n' \
    "$HOST,DNS:$SHORT_HOST" >"$d/server.ext"
  openssl x509 -req -in "$d/server.csr" -CA "$d/ca.crt" -CAkey "$d/ca.key" -CAcreateserial \
    -sha256 -days 825 -extfile "$d/server.ext" -out "$d/server.crt" 2>/dev/null
  rm -f "$d/server.csr" "$d/server.ext" "$d/ca.srl"
}

# --- Repositories -------------------------------------------------------------------------------

write_svnadmin_wrapper() {
  mkdir -p "$VSVN_DIR/bin"
  local u; u="$(apache_user)"
  if [ "$(id -u)" = 0 ]; then
    printf '#!/bin/sh\nexport LANG=C.UTF-8 LC_ALL=C.UTF-8\nexec runuser -u %s -- svnadmin "$@"\n' "$u" >"$VSVN_DIR/bin/svnadmin"
  else
    printf '#!/bin/sh\nexport LANG=C.UTF-8 LC_ALL=C.UTF-8\nexec svnadmin "$@"\n' >"$VSVN_DIR/bin/svnadmin"
  fi
  chmod 755 "$VSVN_DIR/bin/svnadmin"
}

make_repos() {
  local parent="$VSVN_DIR/repos" u
  u="$(apache_user)"
  mkdir -p "$parent"
  chown "$u" "$parent"
  local sa="$VSVN_DIR/bin/svnadmin"
  # Fixed repositories of the layout tests; the conformance suite adds scratch ones.
  for r in "MyRepo" "My Project" "Dự án Tiếng Việt" "Hooked"; do
    [ -d "$parent/$r" ] || "$sa" create "$parent/$r"
  done
  # VisualSVN runs on NTFS: /svn/myrepo opens MyRepo. On a case-sensitive file system the closest
  # emulation is a second name for the same repository directory.
  [ -e "$parent/myrepo" ] || ln -s "MyRepo" "$parent/myrepo"
  # Pre-commit hook that rejects commits without an issue key, with a message for the user.
  cat >"$parent/Hooked/hooks/pre-commit" <<'EOF'
#!/bin/sh
REPOS="$1"; TXN="$2"
MSG=$(/usr/bin/svnlook log -t "$TXN" "$REPOS")
case "$MSG" in
  *ISSUE-[0-9]*) exit 0 ;;
esac
echo "Commit rejected by policy: the log message must reference an issue (ISSUE-123)." >&2
echo "Thông điệp commit phải có mã issue." >&2
exit 1
EOF
  chmod 755 "$parent/Hooked/hooks/pre-commit"
  # A failing post-commit hook: the commit is in, the client only gets a warning (svn prints
  # "Warning: post-commit hook failed").
  cat >"$parent/Hooked/hooks/post-commit" <<'EOF'
#!/bin/sh
REPOS="$1"; REV="$2"
case "$(/usr/bin/svnlook log -r "$REV" "$REPOS")" in
  *POSTFAIL*) echo "post-commit notification failed (mail server down)" >&2; exit 1 ;;
esac
exit 0
EOF
  chmod 755 "$parent/Hooked/hooks/post-commit"
  # Pre-lock policy: files below /nolock/ cannot be locked.
  cat >"$parent/Hooked/hooks/pre-lock" <<'EOF'
#!/bin/sh
case "$2" in
  /nolock/*) echo "Locking files in /nolock is not allowed by policy." >&2; exit 1 ;;
esac
exit 0
EOF
  chmod 755 "$parent/Hooked/hooks/pre-lock"
  # pre-revprop-change permits svn:log edits only.
  cat >"$parent/Hooked/hooks/pre-revprop-change" <<'EOF'
#!/bin/sh
[ "$5" = M ] && [ "$4" = svn:log ] && exit 0
echo "Only svn:log may be changed." >&2
exit 1
EOF
  chmod 755 "$parent/Hooked/hooks/pre-revprop-change"
  chown -R "$u" "$parent"
  # VisualSVN-style authz: everyone reads, groups write, a restricted folder.
  if [ ! -e "$VSVN_DIR/authz" ]; then
    cat >"$VSVN_DIR/authz" <<EOF
[groups]
developers = alice, $BASIC_USER
readers = bob

[/]
* =
@developers = rw
@readers = r

[MyRepo:/secret]
@readers =
EOF
  fi
  chown "$u" "$VSVN_DIR/authz"
  chmod 666 "$VSVN_DIR/authz"
  htpasswd -cbB "$VSVN_DIR/htpasswd" "$BASIC_USER" "$BASIC_PASS" 2>/dev/null
  htpasswd -bB "$VSVN_DIR/htpasswd" bob bob-pass 2>/dev/null
}

# --- Apache -------------------------------------------------------------------------------------

svn_location() {
  # $1 = location path, $2 = auth block, $3 = mod_authz_svn on (1) / off (0)
  cat <<EOF
<Location $1>
  DAV svn
  SVNParentPath $VSVN_DIR/repos
  SVNListParentPath On
  SVNAdvertiseV2Protocol On
  SVNAllowBulkUpdates Prefer
  SVNAutoversioning Off
$2
  Require valid-user
EOF
  if [ "$3" = 1 ]; then echo "  AuthzSVNAccessFile $VSVN_DIR/authz"; fi
  echo "</Location>"
}

GSS_COMMON="  GssapiCredStore keytab:$VSVN_DIR/http.keytab
  GssapiAllowedMech krb5
  GssapiLocalName On
  GssapiSSLonly Off
  GssapiConnectionBound Off"

write_httpd_conf() {
  # $1 = output file, $2 = mod_dav_svn dir, $3 = https port, $4 = http port (or empty), $5 = tag
  local out="$1" moddir="$2" port="$3" http="$4" tag="$5" m=/usr/lib/apache2/modules
  cat >"$out" <<EOF
ServerRoot /etc/apache2
ServerName $HOST
PidFile $VSVN_DIR/httpd-$tag.pid
DefaultRuntimeDir $VSVN_DIR/run-$tag
Mutex file:$VSVN_DIR/run-$tag default
User $(apache_user)
Group $(id -gn "$(apache_user)")
Listen $port https
$( [ -n "$http" ] && echo "Listen $http http" )
LoadModule mpm_event_module     $m/mod_mpm_event.so
LoadModule authn_core_module    $m/mod_authn_core.so
LoadModule authn_file_module    $m/mod_authn_file.so
LoadModule authz_core_module    $m/mod_authz_core.so
LoadModule authz_user_module    $m/mod_authz_user.so
LoadModule auth_basic_module    $m/mod_auth_basic.so
LoadModule auth_gssapi_module   $m/mod_auth_gssapi.so
LoadModule dav_module           $m/mod_dav.so
LoadModule dav_svn_module       $moddir/mod_dav_svn.so
LoadModule authz_svn_module     $moddir/mod_authz_svn.so
LoadModule socache_shmcb_module $m/mod_socache_shmcb.so
LoadModule ssl_module           $m/mod_ssl.so
LoadModule setenvif_module      $m/mod_setenvif.so
ErrorLog $VSVN_DIR/logs/error-$tag.log
LogLevel warn auth_gssapi:debug
# %I / %O: bytes received / sent (request and response, headers included).
LogFormat "%h %u \"%r\" %>s in=%I out=%O %Dus" vsvn
CustomLog $VSVN_DIR/logs/access-$tag.log vsvn
# Large commits (100 MB+ files) and long reports.
LimitRequestBody 0
Timeout 600
# Keep-alive limits (Apache defaults unless set): low values reproduce servers that close or
# recycle pooled connections often (VSVN_KEEPALIVE_TIMEOUT=1 VSVN_MAX_KEEPALIVE_REQUESTS=5).
KeepAlive On
KeepAliveTimeout $KEEPALIVE_TIMEOUT
MaxKeepAliveRequests $MAX_KEEPALIVE_REQUESTS
SSLSessionCache shmcb:$VSVN_DIR/run-$tag/ssl_scache(512000)
SSLProtocol -all +TLSv1.2 +TLSv1.3
# mod_auth_gssapi reads KRB5_CONFIG from Apache's environment (exported by the script).

<VirtualHost *:$port>
  SSLEngine on
  SSLCertificateFile $VSVN_DIR/certs/server.crt
  SSLCertificateKeyFile $VSVN_DIR/certs/server.key

  # VisualSVN "Integrated Windows Authentication" + "Basic authentication": Negotiate first,
  # Basic validated against the KDC (AD) as a fallback. VisualSVN's web listing is the XML
  # index with an XSLT stylesheet.
$(svn_location /svn "  SVNIndexXSLT \"/svnindex.xsl\"
  AuthType GSSAPI
  AuthName \"VisualSVN Server\"
$GSS_COMMON
  GssapiBasicAuth On" 1)

  # IWA only (Basic switched off in VisualSVN): Negotiate is the only offer.
$(svn_location /svn-sso "  AuthType GSSAPI
  AuthName \"VisualSVN Server\"
$GSS_COMMON
  GssapiBasicAuth Off" 1)

  # "Subversion authentication" (htpasswd Basic), as the existing conformance suite expects.
$(svn_location /svn-basic "  AuthType Basic
  AuthName \"VisualSVN Server\"
  AuthBasicProvider file
  AuthUserFile $VSVN_DIR/htpasswd" 1)
</VirtualHost>
EOF
  if [ -n "$http" ]; then
    cat >>"$out" <<EOF

# Plain http with Negotiate offered: the client must refuse to send a token.
<VirtualHost *:$http>
$(svn_location /svn "  AuthType GSSAPI
  AuthName \"VisualSVN Server\"
$GSS_COMMON
  GssapiBasicAuth Off" 0)
</VirtualHost>
EOF
  fi
}

# httpd never calls setlocale(), so mod_dav_svn sees an ASCII locale and cannot list or open
# repositories with non-ASCII names (VisualSVN on Windows has no such limit). A preloaded
# constructor switches the server to the UTF-8 locale of its environment.
build_locale_preload() {
  local so="$VSVN_DIR/bin/utf8-locale.so"
  [ -s "$so" ] && return
  printf '#include <locale.h>\n__attribute__((constructor)) static void f(void) { setlocale(LC_ALL, ""); }\n' \
    >"$VSVN_DIR/bin/utf8-locale.c"
  cc -shared -fPIC -o "$so" "$VSVN_DIR/bin/utf8-locale.c"
}

start_apache() {
  local conf="$1" tag="$2" ld="${3:-}"
  build_locale_preload
  mkdir -p "$VSVN_DIR/logs" "$VSVN_DIR/run-$tag"
  chown "$(apache_user)" "$VSVN_DIR/run-$tag"
  if [ -s "$VSVN_DIR/httpd-$tag.pid" ] && kill -0 "$(cat "$VSVN_DIR/httpd-$tag.pid")" 2>/dev/null; then
    return
  fi
  env KRB5_CONFIG="$VSVN_DIR/krb5.conf" KRB5_KTNAME="$VSVN_DIR/http.keytab" \
    ${ld:+LD_LIBRARY_PATH="$ld"} LD_PRELOAD="$VSVN_DIR/bin/utf8-locale.so" APACHE_RUN_DIR="$VSVN_DIR/run-$tag" \
    /usr/sbin/apache2 -f "$conf" -k start
  for _ in $(seq 50); do [ -s "$VSVN_DIR/httpd-$tag.pid" ] && break; sleep 0.1; done
  [ -s "$VSVN_DIR/httpd-$tag.pid" ] || die "apache ($tag) did not start; see $VSVN_DIR/logs/error-$tag.log"
}

ensure_hosts() {
  local h
  for h in "$HOST" "$SHORT_HOST"; do
    grep -qE "[[:space:]]$h([[:space:]]|\$)" /etc/hosts && continue
    echo "127.0.0.1 $h" | as_root tee -a /etc/hosts >/dev/null
  done
}

cmd_up() {
  mkdir -p "$VSVN_DIR"
  chmod 755 "$VSVN_DIR"
  ensure_hosts
  write_krb5_conf
  setup_realm
  start_kdc
  make_certs
  write_svnadmin_wrapper
  make_repos
  write_httpd_conf "$VSVN_DIR/httpd-14.conf" /usr/lib/apache2/modules "$HTTPS_PORT" "$HTTP_PORT" 14
  start_apache "$VSVN_DIR/httpd-14.conf" 14
  if [ -e "$SVN115_PREFIX/libexec/mod_dav_svn.so" ]; then
    write_httpd_conf "$VSVN_DIR/httpd-15.conf" "$SVN115_PREFIX/libexec" "$HTTPS115_PORT" "" 15
    start_apache "$VSVN_DIR/httpd-15.conf" 15 "$SVN115_PREFIX/lib"
  else
    echo "note: $SVN115_PREFIX/libexec/mod_dav_svn.so missing; only the distribution mod_dav_svn runs" >&2
  fi
  krb_env
  echo "$USER_PASS" | kinit "$USER_PRINC@$REALM" >/dev/null
  echo "up: https://$HOST:$HTTPS_PORT/svn (Negotiate+Basic), /svn-sso, /svn-basic; http://$HOST:$HTTP_PORT/svn"
  echo "ticket for $USER_PRINC@$REALM in $KRB5CCNAME; eval \"\$($0 env)\" before running the tests"
}

cmd_down() {
  for f in "$VSVN_DIR"/httpd-*.pid; do
    [ -s "$f" ] || continue
    kill "$(cat "$f")" 2>/dev/null || true
    rm -f "$f"
  done
  if [ -s "$VSVN_DIR/kdc.pid" ]; then kill "$(cat "$VSVN_DIR/kdc.pid")" 2>/dev/null || true; rm -f "$VSVN_DIR/kdc.pid"; fi
}

cmd_env() {
  cat <<EOF
export KRB5_CONFIG='$VSVN_DIR/krb5.conf'
export KRB5CCNAME='FILE:$VSVN_DIR/ccache'
export OPENSVN_COMPAT_LOCAL_CA='$VSVN_DIR/certs/ca.crt'
export OPENSVN_COMPAT_LOCAL_REPOS='$VSVN_DIR/repos'
export OPENSVN_COMPAT_LOCAL_SVNADMIN='$VSVN_DIR/bin/svnadmin'
export OPENSVN_COMPAT_LOCAL_AUTHZ='$VSVN_DIR/authz'
export OPENSVN_VSVN_HOST='$HOST'
export OPENSVN_VSVN_SHORT_HOST='$SHORT_HOST'
# Keep curl/svn off any HTTP proxy for the test host.
export NO_PROXY="\${NO_PROXY:+\$NO_PROXY,}$HOST,$SHORT_HOST" no_proxy="\${no_proxy:+\$no_proxy,}$HOST,$SHORT_HOST"
export OPENSVN_VSVN_HTTPS_PORT='$HTTPS_PORT'
export OPENSVN_VSVN_HTTP_PORT='$HTTP_PORT'
EOF
  if [ -e "$VSVN_DIR/httpd-15.conf" ]; then echo "export OPENSVN_VSVN_HTTPS115_PORT='$HTTPS115_PORT'"; fi
}

case "${1:-}" in
  install) cmd_install ;;
  up) cmd_up ;;
  down) cmd_down ;;
  env) cmd_env ;;
  purge) cmd_down; rm -rf "$VSVN_DIR" ;;
  *) die "usage: $0 install|up|env|down|purge" ;;
esac
