#!/usr/bin/env bash
# End-to-end updater test against a local static server, no network and no real repository:
#   1. generates a throw-away minisign key pair (never leaves packaging/generated/)
#   2. signs a stand-in artifact, builds latest.json with make-latest-json.mjs
#   3. runs the real tauri-plugin-updater (packaging/updater-harness) against the server:
#      update found + signature verified, no update when current, tampered artifact rejected,
#      wrong public key rejected, tampered or unsigned latest.json rejected, bearer token required
#      and honoured (private-repo behaviour)
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
ensure_cargo
need node "Node 24+"
need pnpm "pnpm 11"

WORK="$GEN_DIR/updater-test"
rm -rf "$WORK"; mkdir -p "$WORK/www"
cd "$REPO_ROOT" || exit 1
[[ -d node_modules ]] || pnpm install --frozen-lockfile >/dev/null

log "building harness (pinned tauri-plugin-updater)"
(cd "$REPO_ROOT/packaging/updater-harness" && cargo build --locked --quiet)
HARNESS="$(cd "$REPO_ROOT/packaging/updater-harness" && cargo metadata --format-version 1 --no-deps | node -e 'process.stdin.on("data",d=>console.log(JSON.parse(d).target_directory))')/debug/updater-harness"
[[ -x "$HARNESS" ]] || die "harness binary missing: $HARNESS"

tauri() { (cd "$APP_DIR" && pnpm --silent tauri "$@"); }
PW="test-password"
log "generating throw-away signing keys"
tauri signer generate --ci -w "$WORK/good.key" -p "$PW" >/dev/null
tauri signer generate --ci -w "$WORK/other.key" -p "$PW" >/dev/null
PUB_GOOD="$(cat "$WORK/good.key.pub")"
PUB_OTHER="$(cat "$WORK/other.key.pub")"

log "creating and signing artifact"
mkdir -p "$WORK/stage/OpenSVN.app/Contents"
head -c 300000 /dev/urandom > "$WORK/stage/OpenSVN.app/Contents/payload.bin"
ARTIFACT="$WORK/www/OpenSVN_0.2.0.app.tar.gz"
tar -czf "$ARTIFACT" -C "$WORK/stage" OpenSVN.app
tauri signer sign -f "$WORK/good.key" -p "$PW" "$ARTIFACT" >/dev/null
[[ -s "$ARTIFACT.sig" ]] || die "signature file was not written"

PLATFORM="$(node -e 'const a={arm64:"aarch64",x64:"x86_64"}[process.arch];const o={darwin:"darwin",linux:"linux",win32:"windows"}[process.platform];console.log(`${o}-${a}`)')"
start_server() { # $1 = optional token, further args go to the server (e.g. --github-accept)
  local token="${1:-}"; shift || true
  rm -f "$WORK/port"
  node "$REPO_ROOT/scripts/release/static-server.mjs" --dir "$WORK/www" --port-file "$WORK/port" ${token:+--token "$token"} "$@" &
  SERVER_PID=$!
  for _ in $(seq 50); do [[ -s "$WORK/port" ]] && break; sleep 0.1; done
  PORT="$(cat "$WORK/port")"
  BASE="http://127.0.0.1:$PORT"
}
stop_server() { kill "$SERVER_PID" 2>/dev/null || true; wait "$SERVER_PID" 2>/dev/null || true; }
trap 'stop_server' EXIT

write_manifest() { # $1 = base URL
  node "$REPO_ROOT/scripts/release/make-latest-json.mjs" --version 0.2.0 --notes "local test" \
    --allow-any-url --out "$WORK/www/latest.json" --entry "$PLATFORM,$ARTIFACT,$1/OpenSVN_0.2.0.app.tar.gz"
}

PASS=0
expect_ok() { # description, expected substring, args...
  local desc="$1" want="$2"; shift 2
  local out; out="$("$HARNESS" "$@" 2>"$WORK/stderr")" || { echo "FAIL  $desc: $(cat "$WORK/stderr")"; exit 1; }
  [[ "$out" == *"$want"* ]] || { echo "FAIL  $desc: got $out"; exit 1; }
  echo "ok    $desc -> $out"; PASS=$((PASS+1))
}
expect_fail() {
  local desc="$1"; shift
  if out="$("$HARNESS" "$@" 2>"$WORK/stderr")"; then echo "FAIL  $desc: unexpectedly succeeded: $out"; exit 1; fi
  echo "ok    $desc -> rejected ($(grep -h '^error:' "$WORK/stderr" | head -1))"; PASS=$((PASS+1))
}

start_server
write_manifest "$BASE"
EP="$BASE/latest.json"
expect_ok "update 0.1.0 (N-1) -> 0.2.0 (N) found, downloaded, signature verified" '"version":"0.2.0"' \
  --endpoint "$EP" --pubkey "$PUB_GOOD" --current 0.1.0 --download
expect_ok "already on 0.2.0: no update" '"available":false' \
  --endpoint "$EP" --pubkey "$PUB_GOOD" --current 0.2.0 --download
expect_fail "signature made by another key is rejected" \
  --endpoint "$EP" --pubkey "$PUB_OTHER" --current 0.1.0 --download
cp "$ARTIFACT" "$ARTIFACT.orig"
printf 'tampered' | dd of="$ARTIFACT" bs=1 seek=1000 conv=notrunc 2>/dev/null
expect_fail "tampered artifact is rejected" \
  --endpoint "$EP" --pubkey "$PUB_GOOD" --current 0.1.0 --download
mv "$ARTIFACT.orig" "$ARTIFACT"

# Tampered or unsigned manifests. The signature lives in latest.json, so editing the manifest
# must be as fatal as editing the artifact.
cp "$WORK/www/latest.json" "$WORK/latest.good.json"
edit_manifest() { # $1 = JS expression run on `m` (the parsed manifest)
  node -e 'const fs=require("fs"),f=process.argv[1],m=JSON.parse(fs.readFileSync(f));'"$1"';fs.writeFileSync(f,JSON.stringify(m))' \
    "$WORK/www/latest.json"
}
OTHER="$WORK/www/OpenSVN_other.app.tar.gz"
head -c 300000 /dev/urandom > "$OTHER"
tauri signer sign -f "$WORK/good.key" -p "$PW" "$OTHER" >/dev/null
OTHER_SIG="$(cat "$OTHER.sig")"
OTHER_SIG="$OTHER_SIG" edit_manifest "for (const p of Object.values(m.platforms)) p.signature = process.env.OTHER_SIG"
expect_fail "latest.json tampered: signature swapped for a valid one of another file" \
  --endpoint "$EP" --pubkey "$PUB_GOOD" --current 0.1.0 --download
cp "$WORK/latest.good.json" "$WORK/www/latest.json"
edit_manifest "for (const p of Object.values(m.platforms)) p.url = p.url.replace(/[^/]+$/, 'OpenSVN_other.app.tar.gz')"
expect_fail "latest.json tampered: url points at another file signed by the same key" \
  --endpoint "$EP" --pubkey "$PUB_GOOD" --current 0.1.0 --download
cp "$WORK/latest.good.json" "$WORK/www/latest.json"
edit_manifest "for (const p of Object.values(m.platforms)) p.signature = p.signature.slice(0, -8) + 'AAAAAAA='"
expect_fail "latest.json tampered: corrupted signature" \
  --endpoint "$EP" --pubkey "$PUB_GOOD" --current 0.1.0 --download
cp "$WORK/latest.good.json" "$WORK/www/latest.json"
edit_manifest "for (const p of Object.values(m.platforms)) p.signature = ''"
expect_fail "latest.json unsigned: empty signature" \
  --endpoint "$EP" --pubkey "$PUB_GOOD" --current 0.1.0 --download
cp "$WORK/latest.good.json" "$WORK/www/latest.json"
edit_manifest "for (const p of Object.values(m.platforms)) delete p.signature"
expect_fail "latest.json unsigned: no signature field" \
  --endpoint "$EP" --pubkey "$PUB_GOOD" --current 0.1.0 --download
cp "$WORK/latest.good.json" "$WORK/www/latest.json"
stop_server

start_server "secret-token"
write_manifest "$BASE"
EP="$BASE/latest.json"
expect_fail "private endpoint without token is refused" \
  --endpoint "$EP" --pubkey "$PUB_GOOD" --current 0.1.0 --download
expect_ok "private endpoint with bearer token works" '"available":true' \
  --endpoint "$EP" --pubkey "$PUB_GOOD" --current 0.1.0 --download \
  --header "Authorization=Bearer secret-token"
stop_server

# GitHub content negotiation: latest.json needs Accept raw+json, the asset needs octet-stream.
start_server "secret-token" --github-accept
write_manifest "$BASE"
EP="$BASE/latest.json"
AUTH="Authorization=Bearer secret-token"
RAW="Accept=application/vnd.github.raw+json"
expect_fail "GitHub semantics: manifest without raw+json Accept is not usable" \
  --endpoint "$EP" --pubkey "$PUB_GOOD" --current 0.1.0 --header "$AUTH"
expect_ok "GitHub semantics: Accept switched to octet-stream for the download" '"available":true' \
  --endpoint "$EP" --pubkey "$PUB_GOOD" --current 0.1.0 --download --header "$AUTH" --header "$RAW"
expect_fail "GitHub semantics: reusing the manifest Accept for the download returns metadata, not the binary" \
  --endpoint "$EP" --pubkey "$PUB_GOOD" --current 0.1.0 --download --download-accept keep \
  --header "$AUTH" --header "$RAW"
# Reference for the app-side check: the token is only sent to https://api.github.com.
expect_fail "download url on a foreign host is refused before any request" \
  --endpoint "$EP" --pubkey "$PUB_GOOD" --current 0.1.0 --download --header "$AUTH" --header "$RAW" \
  --allowed-download-host api.github.com --insecure-test
expect_fail "plain http download url is refused" \
  --endpoint "$EP" --pubkey "$PUB_GOOD" --current 0.1.0 --download --header "$AUTH" --header "$RAW" \
  --allowed-download-host 127.0.0.1
expect_ok "download url on the allowed host passes the check" '"available":true' \
  --endpoint "$EP" --pubkey "$PUB_GOOD" --current 0.1.0 --download --header "$AUTH" --header "$RAW" \
  --allowed-download-host 127.0.0.1 --insecure-test
stop_server
trap - EXIT
log "updater local test: $PASS checks passed"
