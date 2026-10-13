#!/bin/sh
# Start httpd in the foreground. SVN_DUMPIO=1 turns on mod_dumpio request/response capture.
set -eu
if [ ! -s /certs/server.crt ]; then
  echo "missing /certs/server.crt: run the certs service first (cargo xtask compat up)" >&2
  exit 1
fi
chown -R www-data:www-data /var/svn/repos 2>/dev/null || true
flags=""
[ "${SVN_DUMPIO:-0}" = "1" ] && flags="-DDUMPIO"
# shellcheck disable=SC2086
exec apache2 -f /etc/svn-httpd.conf -DFOREGROUND $flags
