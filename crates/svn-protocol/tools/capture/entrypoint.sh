#!/bin/sh
set -e
mkdir -p /repos
if [ ! -d /repos/repo ]; then
  svnadmin create /repos/repo
  htpasswd -bc /repos/htpasswd alice secret >/dev/null
  # Hooks for revprop edits are not needed; keep defaults.
fi
chown -R www-data:www-data /repos
. /etc/apache2/envvars
exec apache2 -D FOREGROUND
