#!/usr/bin/env bash
# Fail when committed fixtures or scenarios reference anything but the synthetic test hosts, or
# carry credentials. Allowed hosts: *.test.local, localhost, 127.0.0.1, example.{com,org,net}.
# Usage: fixtures/check-no-real-hosts.sh   (run from anywhere)
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
paths=(fixtures tests/compat/scenarios)
status=0

# Any URL host that is not on the allowlist.
bad_urls=$(grep -RIEho --exclude='check-no-real-hosts.sh' 'https?://[^/[:space:]"'"'"')>]+' "${paths[@]}" 2>/dev/null \
  | sed -E 's|https?://||; s|^[^@]*@||; s|:[0-9]+$||' \
  | grep -Ev '^(([a-z0-9-]+\.)*test\.local|localhost|127\.0\.0\.1|example\.(com|org|net)|\{[a-z]+\}.*)$' \
  | sort -u || true)
if [ -n "$bad_urls" ]; then
  echo "non-allowlisted hosts in fixtures/scenarios:" >&2
  echo "$bad_urls" >&2
  status=1
fi

# Credentials: auth headers or password/token assignments.
if grep -RIinE --exclude='check-no-real-hosts.sh' '^(authorization|proxy-authorization|cookie):|password[[:space:]]*[:=]|BEGIN [A-Z ]*PRIVATE KEY' "${paths[@]}" 2>/dev/null; then
  echo "credential-like content found in fixtures/scenarios" >&2
  status=1
fi
[ "$status" -eq 0 ] && echo "fixtures clean"
exit "$status"
