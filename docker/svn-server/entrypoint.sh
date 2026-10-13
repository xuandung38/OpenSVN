#!/bin/sh
# Start httpd in the foreground. SVN_DUMPIO=1 turns on mod_dumpio request/response capture.
set -eu
if [ ! -s /certs/server.crt ]; then
  echo "missing /certs/server.crt: run the certs service first (cargo xtask compat up)" >&2
  exit 1
fi
chown -R www-data:www-data /var/svn/repos 2>/dev/null || true
# Pre-create the logs owned by the invoking user; httpd (root at open time) appends to them
# without changing ownership. Best effort: bind mounts on macOS ignore chown.
touch /var/log/svn-httpd/error.log /var/log/svn-httpd/access.log
chown "${HOST_UID:-0}:${HOST_GID:-0}" /var/log/svn-httpd /var/log/svn-httpd/*.log 2>/dev/null || true
flags=""
[ "${SVN_DUMPIO:-0}" = "1" ] && flags="-DDUMPIO"
# shellcheck disable=SC2086
exec apache2 -f /etc/svn-httpd.conf -DFOREGROUND $flags
