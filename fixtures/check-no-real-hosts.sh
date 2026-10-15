#!/usr/bin/env bash
# Fail when committed fixtures or scenarios reference anything but reserved test hosts, or carry
# credentials. Allowed hosts: names under the reserved test TLDs (.test, .example, .invalid,
# .localhost) and *.test.local, localhost, 127.0.0.1, [::1], and example.{com,net,org} with their
# subdomains.
# Usage: fixtures/check-no-real-hosts.sh               (run from anywhere)
#        fixtures/check-no-real-hosts.sh --self-test   (checks the host filter itself)
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
paths=(fixtures tests/compat/scenarios)

# URLs on stdin, one per line; prints the host of each URL that is not allowed. The host ends at
# the first `/`, `\`, `?` or `#` (WHATWG parsers treat `\` as `/` in http(s) URLs), so a
# Windows-style path after a test host is not part of the host. Userinfo and a port (a number,
# `$VAR` or `{name}` in scripts) are dropped first. `{name}` hosts are template placeholders.
disallowed_hosts() {
  sed -E 's|^[A-Za-z]+://||; s|[/\\?#].*$||; s|^.*@||; s|:([0-9]*\|\$[A-Za-z_]+\|\{[a-z]+\})$||' \
    | tr 'A-Z' 'a-z' \
    | grep -Ev '^(([a-z0-9-]+\.)*(test|example|invalid|localhost|test\.local)\.?|([a-z0-9-]+\.)*example\.(com|org|net)\.?|localhost|127\.0\.0\.1|\[::1\]|\{[a-z]+\}.*)$' \
    || true
}

self_test() {
  local failed=0 url got
  # Allowed: reserved test domains, whatever follows the host.
  for url in \
    'http://svn.test.local\svn\repo\..' \
    'http://svn.test.local\\svn\\repo\\..' \
    'https://svn.test.local/svn/repo' \
    'http://evil.test.local:8080/x' \
    'https://svn.opensvn.test:18443/svn' \
    'https://SVN.OpenSVN.TEST/svn' \
    'https://repo.example/svn' \
    'https://nothing.invalid' \
    'http://app.localhost:1420/' \
    'https://example.com/svn' \
    'https://svn.example.net\repo' \
    'https://www.example.org?q#f' \
    'http://user@svn.test.local/repo' \
    'http://localhost:$PORT/repo' \
    'http://127.0.0.1:{port}/repo' \
    'http://[::1]:8080/repo' \
    'http://{host}/repo'; do
    got=$(printf '%s\n' "$url" | disallowed_hosts)
    if [ -n "$got" ]; then echo "self-test: $url should be allowed, got: $got" >&2; failed=1; fi
  done
  # Refused: any other host, also when it hides behind a test name.
  for url in \
    'https://svn.qzxv-sample.co/svn/repo' \
    'https://code.qzxv-sample.co\svn\repo' \
    'http://svn.qzxv-sample.example.co\svn' \
    'https://svn.test.local.qzxv-sample.co/repo' \
    'https://example.com.qzxv-sample.net/repo' \
    'https://qzxvexample.com/repo' \
    'https://svn.test.local@qzxv-sample.co/repo' \
    'https://svn.testlocal/repo' \
    'https://qzxv-sample.co?svn.test.local' \
    'https://qzxv-sample.co#.test' \
    'http://svnhost/svn' \
    'http://10.0.0.5:8080/svn'; do
    got=$(printf '%s\n' "$url" | disallowed_hosts)
    if [ -z "$got" ]; then echo "self-test: $url should be refused" >&2; failed=1; fi
  done
  [ "$failed" -eq 0 ] && echo "self-test passed"
  return "$failed"
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi
self_test >/dev/null || { echo "host filter self-test failed; run with --self-test" >&2; exit 1; }

status=0
# Any URL host that is not on the allowlist. XML namespace names (DAV/svn/XML Schema) are URIs,
# not hosts anything connects to, so they are dropped first.
bad_urls=$(grep -RIEho --exclude='check-no-real-hosts.sh' 'https?://[^[:space:]"'"'"')>]+' "${paths[@]}" 2>/dev/null \
  | grep -Ev '^http://(subversion\.tigris\.org/xmlns/|apache\.org/dav/(props|propset)/|www\.w3\.org/)' \
  | disallowed_hosts | sort -u || true)
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
