#!/usr/bin/env bash
# Checks that the updater read token (the one provisioned into client keychains) can read the
# releases repository and nothing else:
#   1. GET  repos/<releases>                      200  (the token sees the releases repository)
#   2. GET  repos/<releases>/contents/latest.json 200  (404 accepted: latest.json not published yet)
#   3. GET  repos/<main>                          404  (the source repository stays invisible)
#   4. git ls-remote <main>                       fails (no git read of the source either)
#   5. PUT  repos/<releases>/contents/<probe>     403  (no write)
#
# Usage (the token is read from the environment and never printed or put on a command line):
#   read -rs OPENSVN_UPDATER_TOKEN && export OPENSVN_UPDATER_TOKEN
#   scripts/release/check-token-scope.sh
#
# The write probe can never change the repository: it targets a branch that does not exist, so a
# token that may write gets a 404/422 (reported as FAIL) instead of a commit.
#
# Exit status: 0 every check passed, 1 at least one failed, 2 usage error.
# Overrides (tests): GITHUB_API (https://api.github.com), GITHUB_GIT (https://github.com),
# MAIN_REPO (xuandung38/OpenSVN), RELEASES_REPO (xuandung38/OpenSVN-releases).
set -uo pipefail

API="${GITHUB_API:-https://api.github.com}"
GIT_BASE="${GITHUB_GIT:-https://github.com}"
MAIN_REPO="${MAIN_REPO:-xuandung38/OpenSVN}"
RELEASES_REPO="${RELEASES_REPO:-xuandung38/OpenSVN-releases}"
PROBE_BRANCH="opensvn-token-scope-probe-does-not-exist"

if [[ -z "${OPENSVN_UPDATER_TOKEN:-}" ]]; then
  echo "usage: OPENSVN_UPDATER_TOKEN must hold the updater read token (read -rs, then export)" >&2
  exit 2
fi
command -v curl >/dev/null || { echo "curl is required" >&2; exit 2; }
command -v git >/dev/null || { echo "git is required" >&2; exit 2; }

failed=0
pass() { printf 'PASS  %s\n' "$1"; }
note() { printf 'PASS  %s\n      note: %s\n' "$1" "$2"; }
fail() { printf 'FAIL  %s\n      %s\n' "$1" "$2"; failed=1; }

# HTTP status of an API request. The Authorization header goes to curl through its config on
# stdin (printf is a shell builtin), so the token never shows up in the process list.
status() {
  local method="$1" path="$2" body="${3:-}"
  local args=(-sS -o /dev/null -w '%{http_code}' -X "$method" --max-time 30 -K -
    -H 'Accept: application/vnd.github+json' -H 'X-GitHub-Api-Version: 2022-11-28'
    -A 'opensvn-check-token-scope')
  [[ -n "$body" ]] && args+=(-H 'Content-Type: application/json' --data "$body")
  printf 'header = "Authorization: Bearer %s"\n' "$OPENSVN_UPDATER_TOKEN" \
    | curl "${args[@]}" "$API/$path" 2>/dev/null || true
}

# `git ls-remote` of a repository with the token, through environment-only git config (no argv,
# no credential helper, no prompt). Exit status of git.
ls_remote() {
  local basic
  basic="$(printf 'x-access-token:%s' "$OPENSVN_UPDATER_TOKEN" | base64 | tr -d '\n')"
  GIT_TERMINAL_PROMPT=0 GIT_ASKPASS=true SSH_ASKPASS=true GIT_CONFIG_NOSYSTEM=1 \
    GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_COUNT=4 \
    GIT_CONFIG_KEY_0=http.extraHeader GIT_CONFIG_VALUE_0="Authorization: Basic $basic" \
    GIT_CONFIG_KEY_1=credential.helper GIT_CONFIG_VALUE_1="" \
    GIT_CONFIG_KEY_2=http.lowSpeedLimit GIT_CONFIG_VALUE_2=1 \
    GIT_CONFIG_KEY_3=http.lowSpeedTime GIT_CONFIG_VALUE_3=30 \
    git ls-remote --heads "$GIT_BASE/$1.git" >/dev/null 2>&1
}

unreachable() { echo "no HTTP answer from $API (network, proxy or DNS problem; nothing was checked)"; }

echo "Token scope check: read-only on $RELEASES_REPO, no access to $MAIN_REPO"

# 1. The releases repository is visible.
code="$(status GET "repos/$RELEASES_REPO")"
case "$code" in
  200) pass "GET repos/$RELEASES_REPO -> 200" ;;
  000) fail "GET repos/$RELEASES_REPO" "$(unreachable)" ;;
  401) fail "GET repos/$RELEASES_REPO -> 401" "the token is invalid or expired" ;;
  *) fail "GET repos/$RELEASES_REPO -> $code" \
       "the token cannot see the releases repository; select it under 'Only select repositories'" ;;
esac
releases_visible=$([[ "$code" == 200 ]] && echo 1 || echo 0)

# 2. latest.json is readable (or simply not published yet).
code="$(status GET "repos/$RELEASES_REPO/contents/latest.json")"
case "$code" in
  200) pass "GET repos/$RELEASES_REPO/contents/latest.json -> 200" ;;
  404)
    if [[ "$releases_visible" == 1 ]]; then
      note "GET repos/$RELEASES_REPO/contents/latest.json -> 404" \
        "latest.json does not exist yet (expected before the first stable release); the repository itself is readable"
    else
      fail "GET repos/$RELEASES_REPO/contents/latest.json -> 404" \
        "the releases repository is not visible to the token (see the first check)"
    fi ;;
  000) fail "GET repos/$RELEASES_REPO/contents/latest.json" "$(unreachable)" ;;
  *) fail "GET repos/$RELEASES_REPO/contents/latest.json -> $code" \
       "the token needs Contents: Read-only on the releases repository" ;;
esac

# 3. The source repository is invisible through the API.
code="$(status GET "repos/$MAIN_REPO")"
case "$code" in
  404) pass "GET repos/$MAIN_REPO -> 404" ;;
  000) fail "GET repos/$MAIN_REPO" "$(unreachable)" ;;
  *) fail "GET repos/$MAIN_REPO -> $code (want 404)" \
       "the token reaches the source repository; restrict it to $RELEASES_REPO only" ;;
esac

# 4. The source repository cannot be read over git. The releases repository is the control: if
# git cannot reach it either, a failing ls-remote of the source proves nothing.
if ls_remote "$MAIN_REPO"; then
  fail "git ls-remote $MAIN_REPO succeeded (want failure)" \
    "the token can clone the source repository; restrict it to $RELEASES_REPO only"
elif ls_remote "$RELEASES_REPO"; then
  pass "git ls-remote $MAIN_REPO fails (control: git ls-remote $RELEASES_REPO works)"
else
  fail "git ls-remote $MAIN_REPO fails, but so does git ls-remote $RELEASES_REPO" \
    "inconclusive: git could not use the token at all (network, proxy, or the releases repository has no commits)"
fi

# 5. No write to the releases repository.
probe_body="{\"message\":\"token scope probe\",\"content\":\"cHJvYmUK\",\"branch\":\"$PROBE_BRANCH\"}"
code="$(status PUT "repos/$RELEASES_REPO/contents/.opensvn-token-scope-probe" "$probe_body")"
case "$code" in
  403) pass "PUT repos/$RELEASES_REPO/contents/... -> 403" ;;
  000) fail "PUT repos/$RELEASES_REPO/contents/..." "$(unreachable)" ;;
  401) fail "PUT repos/$RELEASES_REPO/contents/... -> 401" "the token is invalid or expired" ;;
  200|201) fail "PUT repos/$RELEASES_REPO/contents/... -> $code (want 403)" \
       "the token can WRITE: it created .opensvn-token-scope-probe; delete that file and give the token Contents: Read-only" ;;
  *)
    if [[ "$releases_visible" == 1 ]]; then
      fail "PUT repos/$RELEASES_REPO/contents/... -> $code (want 403)" \
        "the request got past the permission check, so the token can write; give it Contents: Read-only"
    else
      fail "PUT repos/$RELEASES_REPO/contents/... -> $code (want 403)" \
        "inconclusive: the releases repository is not visible to the token (see the first check)"
    fi ;;
esac

if [[ "$failed" == 0 ]]; then
  echo "RESULT: PASS (read-only on $RELEASES_REPO, nothing else)"
else
  echo "RESULT: FAIL (fix the token scope before provisioning it to clients)"
fi
exit "$failed"
