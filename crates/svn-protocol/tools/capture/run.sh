#!/usr/bin/env bash
# Records raw svn <-> mod_dav_svn traffic into $OUT/<step>.jsonl via proxy.py.
# Requires: docker, svn client, python3. Usage: ./run.sh OUT_DIR
set -euo pipefail
export LC_ALL=en_US.UTF-8
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="${1:?out dir}"; mkdir -p "$OUT"; OUT="$(cd "$OUT" && pwd)"
WORK="$(mktemp -d)"; trap 'kill ${PROXY:-} 2>/dev/null || true; docker rm -f opensvn-capture >/dev/null 2>&1 || true' EXIT
docker image inspect opensvn-capture >/dev/null 2>&1 || docker build -q -t opensvn-capture "$HERE" >/dev/null
docker rm -f opensvn-capture >/dev/null 2>&1 || true
docker run -d --name opensvn-capture -p 127.0.0.1:18080:80 opensvn-capture >/dev/null
sleep 3
docker exec opensvn-capture htpasswd -b /repos/htpasswd bob hunter2 >/dev/null
docker exec opensvn-capture sh -c 'echo "#!/bin/sh
exit 0" > /repos/repo/hooks/pre-revprop-change && chmod +x /repos/repo/hooks/pre-revprop-change'
docker exec opensvn-capture apache2ctl -v | head -1 > "$OUT/server-version.txt"
docker exec opensvn-capture svnadmin --version | head -1 >> "$OUT/server-version.txt"
python3 "$HERE/proxy.py" 18081 18080 "$WORK/cur.jsonl" & PROXY=$!
sleep 1
U=http://127.0.0.1:18081/svn/repo
A=(--username alice --password secret --non-interactive --no-auth-cache)
B=(--username bob --password hunter2 --non-interactive --no-auth-cache)
step() { name="$1"; shift; : > "$WORK/cur.jsonl"; "$@" >"$OUT/$name.svn-out.txt" 2>&1 || true; mv "$WORK/cur.jsonl" "$OUT/$name.jsonl"; }
svn "${A[@]}" mkdir -m "layout" $U/trunk $U/branches >/dev/null
WC="$WORK/wc"; svn "${A[@]}" checkout -q $U/trunk "$WC"
mkdir -p "$WC/src/deep" "$WC/docs"
printf 'hello\nworld\n' > "$WC/a.txt"; printf 'second file\n' > "$WC/src/b.txt"
printf 'x=1\n' > "$WC/src/deep/c.cfg"; printf 'readme\n' > "$WC/docs/readme.md"
printf 'caf\xc3\xa9 \xe2\x82\xac\n' > "$WC/"$'unicod\xc3\xa9 file.txt'
svn add -q "$WC"/* >/dev/null; svn propset -q svn:executable '*' "$WC/src/b.txt"; svn propset -q custom:tag 'v<1>&"q"' "$WC/a.txt"
svn "${A[@]}" commit -q -m 'initial: files & props <&> unicode é' "$WC" >/dev/null
printf 'hello\nworld\nmore\n' > "$WC/a.txt"; svn "${A[@]}" commit -q -m 'edit a.txt' "$WC" >/dev/null
svn "${A[@]}" rm -q "$U/trunk/docs/readme.md" -m 'delete readme' >/dev/null
svn "${A[@]}" cp -q "$U/trunk/src" "$U/branches/b1" -m 'branch b1 copy' >/dev/null
svn "${A[@]}" up -q "$WC" >/dev/null
step options-info svn "${A[@]}" info $U
HEADREV=$(svn "${A[@]}" info --show-item revision $U)
step list-propfind-depth1 curl -sS -u alice:secret -X PROPFIND -H 'Depth: 1' -H 'Content-Type: text/xml' --data '<?xml version="1.0" encoding="utf-8"?><propfind xmlns="DAV:"><prop><resourcetype xmlns="DAV:"/><getcontentlength xmlns="DAV:"/><deadprop-count xmlns="http://subversion.tigris.org/xmlns/dav/"/><version-name xmlns="DAV:"/><creationdate xmlns="DAV:"/><creator-displayname xmlns="DAV:"/></prop></propfind>' $U/!svn/rvr/$HEADREV/trunk
step stat-dir svn "${A[@]}" info $U/trunk/src
step stat-file svn "${A[@]}" info $U/trunk/a.txt
step stat-missing svn "${A[@]}" info $U/trunk/nope.txt
step list-trunk svn "${A[@]}" list -v $U/trunk
step list-peg svn "${A[@]}" list -v -r2 $U/trunk
step proplist svn "${A[@]}" proplist -v $U/trunk/a.txt
step cat svn "${A[@]}" cat $U/trunk/a.txt
step cat-peg svn "${A[@]}" cat -r2 $U/trunk/a.txt
step cat-missing svn "${A[@]}" cat $U/trunk/nope.txt
step log-verbose svn "${A[@]}" log -v $U/trunk
step log-limit svn "${A[@]}" log --xml -v -l 2 --with-all-revprops $U
step log-range svn "${A[@]}" log -r 3:1 $U/trunk/a.txt
step log-empty-range svn "${A[@]}" log -r 2:2 -q $U
step lock svn "${A[@]}" lock -m 'locking <it>' $U/trunk/a.txt
step lock-info svn "${A[@]}" info $U/trunk/a.txt
step list-locks svn "${A[@]}" list -v $U/trunk
step lock-conflict svn "${B[@]}" lock $U/trunk/a.txt
step lock-steal svn "${B[@]}" lock --force -m stolen $U/trunk/a.txt
step unlock-break svn "${A[@]}" unlock --force $U/trunk/a.txt
step lock-b1 svn "${A[@]}" lock $U/trunk/src/b.txt
step unlock svn "${A[@]}" unlock $U/trunk/src/b.txt
step auth-fail svn --username alice --password wrong --non-interactive --no-auth-cache info $U
step checkout svn "${A[@]}" checkout $U/trunk "$WORK/wc2"
step checkout-skelta svn "${A[@]}" --config-option servers:global:http-bulk-updates=no checkout $U/trunk "$WORK/wc3"
printf 'hello\nworld\nmore\nand more\n' > "$WC/a.txt"; mkdir "$WC/newdir"; echo n > "$WC/newdir/n.txt"; svn add -q "$WC/newdir"
step commit svn "${A[@]}" commit -m 'second commit: add dir & edit' "$WC"
step update svn "${A[@]}" update "$WORK/wc2"
step status-u svn "${A[@]}" status -u "$WORK/wc3"
step switch svn "${A[@]}" switch $U/branches/b1 "$WORK/wc3"

# --- additional scenarios (run last so earlier revision numbers stay stable) ---
step log-xml-verbose svn "${A[@]}" log --xml -v $U/trunk
step log-xml-root svn "${A[@]}" log --xml -v $U
step list-xml svn "${A[@]}" list --xml $U/trunk
step info-xml svn "${A[@]}" info --xml $U/trunk/a.txt
# merge history: change branch, merge into trunk, log with merged revisions
BW="$WORK/bw"; svn "${A[@]}" checkout -q $U/branches/b1 "$BW"
echo "branch edit" >> "$BW/b.txt"; svn "${A[@]}" commit -q -m 'edit on branch' "$BW" >/dev/null
TW="$WORK/tw"; svn "${A[@]}" checkout -q $U/trunk "$TW"
svn "${A[@]}" merge -q $U/branches/b1 "$TW/src" >/dev/null 2>&1 || true
svn "${A[@]}" commit -q -m 'merge b1 into trunk' "$TW" >/dev/null || true
step log-merged svn "${A[@]}" log --xml -v -g -l 3 $U/trunk
# out-of-date commit and commit on a path locked by someone else
W1="$WORK/o1"; W2="$WORK/o2"; svn "${A[@]}" checkout -q $U/trunk "$W1"; svn "${A[@]}" checkout -q $U/trunk "$W2"
echo one >> "$W1/a.txt"; svn "${A[@]}" commit -q -m 'first' "$W1" >/dev/null
echo two >> "$W2/a.txt"
step commit-out-of-date svn "${A[@]}" commit -m 'stale' "$W2"
svn "${A[@]}" lock -q $U/trunk/src/b.txt >/dev/null
echo lk >> "$W2/src/b.txt"; svn revert -q "$W2/a.txt"
step commit-locked-by-other svn "${B[@]}" commit -m 'locked' "$W2"
svn "${A[@]}" unlock -q $U/trunk/src/b.txt >/dev/null
step get-locks-infinity svn "${A[@]}" status -u "$W1"
# lock from a working copy (sends the working revision) and force-unlock of an unlocked path
WCL="$WORK/wcl"; svn "${A[@]}" checkout -q $U/trunk "$WCL"
step lock-wc svn "${A[@]}" lock -m 'from wc' "$WCL/a.txt"
step unlock-wc svn "${A[@]}" unlock "$WCL/a.txt"
step unlock-force-unlocked svn "${A[@]}" unlock --force $U/trunk/src/deep/c.cfg
# --- richer commit / update / blame scenarios ---
step blame svn "${A[@]}" blame --xml $U/trunk/a.txt
CW="$WORK/cw"; svn "${A[@]}" checkout -q $U/trunk "$CW"
svn propset -q svn:eol-style native "$CW/a.txt"; svn propset -q custom:note 'x<y' "$CW/src"
svn rm -q "$CW/src/b.txt"; svn cp -q "$CW/a.txt" "$CW/copy.txt"
mkdir "$CW/fresh"; printf 'fresh\n' > "$CW/fresh/f.txt"; printf '' > "$CW/fresh/empty.txt"; svn add -q "$CW/fresh"
printf 'edited\nlines\n' >> "$CW/docs/../a.txt"
step commit-full svn "${A[@]}" commit -m 'props, delete, copy, add' "$CW"
svn "${A[@]}" up -q "$CW" >/dev/null
svn "${A[@]}" lock -q "$CW/copy.txt" >/dev/null
echo more >> "$CW/copy.txt"
step commit-with-lock svn "${A[@]}" commit -m 'edit locked file' "$CW"
step update-rich svn "${A[@]}" update "$WORK/wc2"
step update-rich-skelta svn "${A[@]}" --config-option servers:global:http-bulk-updates=no update "$WORK/wc3"
step switch-trunk svn "${A[@]}" switch $U/trunk "$WORK/wc3"
step status-u-rich svn "${A[@]}" status -u "$WORK/wc2"
# mixed working copy: switched subtree, locked file, missing directory, sparse depth
MW="$WORK/mw"; svn "${A[@]}" checkout -q $U/trunk "$MW"
svn "${A[@]}" switch -q $U/branches/b1 "$MW/src" >/dev/null 2>&1 || true
svn "${A[@]}" lock -q "$MW/a.txt" >/dev/null || true
rm -rf "$MW/docs" "$MW/newdir"
echo bump >> "$CW/a.txt"; svn "${A[@]}" commit -q -m 'bump for mixed update' "$CW" >/dev/null || true
step update-mixed svn "${A[@]}" update "$MW"
SW="$WORK/sw"; svn "${A[@]}" checkout -q --depth immediates $U/trunk "$SW"
step update-sparse svn "${A[@]}" update --set-depth infinity "$SW/fresh"
# commit message with control characters: the server sends it base64-encoded (encode-binary-props).
# (svn refuses control characters in lock comments, so locks cannot be recorded that way.)
printf 'esc \033[1mbold\033[0m message\n' > "$WORK/esc-msg.txt"
CC="$WORK/cc"; svn "${A[@]}" checkout -q $U/trunk "$CC"
echo esc >> "$CC/src/b.txt"
step commit-control-chars svn "${A[@]}" commit -q -F "$WORK/esc-msg.txt" "$CC"
step log-control-chars svn "${A[@]}" log --xml -v -l 1 $U/trunk
echo "done"; ls "$OUT"
