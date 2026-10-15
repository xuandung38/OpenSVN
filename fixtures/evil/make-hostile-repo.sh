#!/usr/bin/env bash
# Hostile repository: names, properties, log messages and content written to attack a client UI
# (script injection, link and network beacons, bidi spoofing), symlinks that escape the working
# copy, absurdly long names, and invalid UTF-8 everywhere svn stores it. Every case is listed in
# fixtures/evil/README.md ("Hostile repository").
#
# Usage: fixtures/evil/make-hostile-repo.sh <repo-path>
# Env:   BEACON  URL prefix every network beacon points at (default https://beacon.evil.test/).
#                The e2e test points it at a local listener and asserts that it gets no request.
# Needs: svn, svnadmin, svnmucc (1.8+, svn 1.15.0 recommended: `nix-build
#        docker/nix/subversion-1.15.nix`), python3. Linux or macOS (symlinks, `<>` in names).
#
# Layout: trunk/ is checked out by the e2e test and must stay checkout-able by a careful client
# on Linux; names/ holds raw-markup and Windows-invalid names (a careful client refuses to check
# it out); branches/ and tags/ hold what no checkout can materialise (names over 255 bytes, an
# external to a hostile URL) and hostile branch/tag names for the Branch menu.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../repos/lib.sh
. "$here/../repos/lib.sh"

repo="${1:?usage: make-hostile-repo.sh <repo-path>}"
BEACON="${BEACON:-https://beacon.evil.test/}"
BEACON="${BEACON%/}"
repo="$(mkdir -p "$(dirname "$repo")" && cd "$(dirname "$repo")" && pwd)/$(basename "$repo")"
url="file://$repo"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
fx_create "$repo" "0e5f1a2b-3c4d-4e5f-8a6b-7c8d9e0f1a2b"
# Hostile authors are set at the end with `svnadmin setrevprop`; commits run as "fixture".
U=(--username fixture --non-interactive)
commit() { svn commit -q "${U[@]}" -F "$1" "$wc"; svn update -q "$wc"; }
msg() { printf '%s' "$1" > "$work/msg"; echo "$work/msg"; }

svn checkout -q "$url" "$work/wc"
wc="$work/wc"
t="$wc/trunk"

# r1: layout. The log message is a script element.
svn mkdir -q "$t" "$wc/branches" "$wc/tags"
commit "$(msg "Layout <script>alert('log r1')</script>")"

# r2: hostile names. trunk/ keeps the ones a careful client still writes to disk (template,
# entity and percent-encoded markup, bidi override); names/ holds raw markup and names that are
# invalid or dangerous on Windows (`<>"|?*:\`, trailing dot/space, device names), which a client
# must show as text in listings and refuse to create on disk.
printf 'This repository attacks SVN clients. See fixtures/evil/README.md.\n' > "$t/README.txt"
trunk_names=(
  "{{constructor.constructor('alert(1)')()}}.txt"
  "\${alert('name')}.txt"
  "'-alert('name')-'.txt"
  "&lt;img src=x onerror=alert('entity')&gt;.txt"
  "%3Cscript%3Ealert('percent')%3C%2Fscript%3E.txt"
  $'invoice‮txt.exe'
)
names_dir="$wc/names"
raw_names=(
  "<img src=x onerror=alert('name')>.txt"
  "<script>alert('name')<%2Fscript>.txt"
  "\"><iframe src=javascript:alert('name')>.txt"
  "javascript:alert('name').txt"
  "back\\slash.txt"
  " leading and trailing space "
  "trailing dot."
  "CON.txt"
  "pipe|star*question?.txt"
)
mkdir "$names_dir" "$names_dir/<svg onload=alert('dir')>"
printf 'inside a hostile directory\n' > "$names_dir/<svg onload=alert('dir')>/inner.txt"
for n in "${trunk_names[@]}"; do printf 'content of a hostile name\n' > "$t/$n"; done
for n in "${raw_names[@]}"; do printf 'content of a hostile name\n' > "$names_dir/$n"; done
svn add -q --depth empty "$names_dir"
svn add -q "$t/README.txt" "$names_dir/<svg onload=alert('dir')>"
for n in "${trunk_names[@]}"; do svn add -q "$t/$n"; done
for n in "${raw_names[@]}"; do svn add -q "$names_dir/$n"; done
commit "$(msg "<img src=x onerror=alert('log r2')> hostile names")"

# r3: active content. SVG and HTML with scripts, event handlers and beacons.
cat > "$t/evil.svg" <<EOF
<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="64" height="64" onload="alert('svg onload')">
  <script>alert('svg script'); top.__opensvnPwned = 'svg';</script>
  <image href="$BEACON/svg-image.png" xlink:href="$BEACON/svg-xlink.png" width="1" height="1"/>
  <foreignObject width="64" height="64"><iframe xmlns="http://www.w3.org/1999/xhtml" src="javascript:alert('svg iframe')"></iframe></foreignObject>
  <a href="javascript:alert('svg link')"><rect width="64" height="64" fill="red"/></a>
</svg>
EOF
cat > "$t/evil.html" <<EOF
<!doctype html>
<meta http-equiv="refresh" content="0;url=javascript:alert('refresh')">
<link rel="stylesheet" href="$BEACON/html.css">
<script>alert('html script'); top.__opensvnPwned = 'html';</script>
<img src="$BEACON/html-img.png" onerror="alert('html onerror')">
<iframe src="$BEACON/html-frame"></iframe>
EOF
printf '<img src=x onerror=alert(1)>\n<script>alert(2)</script>\n%s/text.png\n' "$BEACON" > "$t/xss-content.txt"
# Markdown that renders to active links/images if anything renders it.
printf '[click](javascript:alert(1))\n![beacon](%s/md.png)\n<details open ontoggle=alert(1)>\n' \
  "$BEACON" > "$t/evil.md"
svn add -q "$t/evil.svg" "$t/evil.html" "$t/xss-content.txt" "$t/evil.md"
svn propset -q svn:mime-type image/svg+xml "$t/evil.svg"
svn propset -q svn:mime-type text/html "$t/evil.html"
# A text file that claims to be HTML: a client that renders by mime type would run it.
svn propset -q svn:mime-type text/html "$t/xss-content.txt"
commit "$(msg "[click me](javascript:alert('log r3')) <a href=\"javascript:alert(1)\">x</a> $BEACON/log.png")"

# r4: hostile property values (names are restricted by svn to XML name characters).
f="$t/props.txt"
printf 'carries hostile property values\n' > "$f"
svn add -q "$f"
for target in "$t" "$f" "$t/evil.svg" "$names_dir/<img src=x onerror=alert('name')>.txt"; do
  svn propset -q custom:xss "<script>alert('prop')</script>" "$target"
  svn propset -q custom:img "<img src=x onerror=alert('prop')>" "$target"
  svn propset -q custom:link "javascript:alert('prop')" "$target"
  svn propset -q custom:beacon "<img src=\"$BEACON/prop.png\">" "$target"
done
# Binary custom values are stored as given: `-F` skips the native-encoding conversion.
printf '\xff\xfe<script>alert("bad utf-8 prop")</script>\x00\x01' > "$work/bad"
svn propset -q custom:bad-utf8 -F "$work/bad" "$f"
python3 -c 'import sys; sys.stdout.write("<b>" * 16384)' > "$work/huge"
svn propset -q custom:huge -F "$work/huge" "$f"
svn propset -q svn:ignore "<script>alert('ignore')</script>" "$t"
commit "$(msg $'Properties\n\x1b[31mANSI red\x1b[0m ‮eulav desrever‬')"

# r5: symlinks that leave the working copy (svn:special). svn itself creates them on checkout.
ln -s ../../../../../../../../../../../../etc/passwd "$t/escape-up"
ln -s /etc/passwd "$t/escape-abs"
ln -s /etc "$t/escape-dir"
ln -s ../.. "$t/escape-parent"
ln -s escape-loop "$t/escape-loop"
ln -s ".svn/wc.db" "$t/into-admin"
svn add -q "$t/escape-up" "$t/escape-abs" "$t/escape-dir" "$t/escape-parent" "$t/escape-loop" \
  "$t/into-admin"
commit "$(msg "Symlinks out of the working copy")"

# r6: absurdly long names that still fit Linux/macOS limits (255 bytes per name, deep path).
long="$(printf 'L%.0s' $(seq 251)).txt"
printf 'long name\n' > "$t/$long"
deep="$t/deep"
for _ in $(seq 8); do deep="$deep/$(printf 'D%.0s' $(seq 200))"; done
mkdir -p "$deep"
printf 'deep\n' > "$deep/leaf.txt"
svn add -q "$t/$long" "$t/deep"
python3 -c 'import sys; sys.stdout.write("A very long log message. " * 10923)' > "$work/longlog"
commit "$work/longlog"

# r7: what a checkout cannot materialise lives outside trunk: names over NAME_MAX and a hostile
# external URL; plus hostile branch and tag names.
svnmucc "${U[@]}" -m "Too-long names" \
  mkdir "$url/branches/too-long" \
  put /dev/null "$url/branches/too-long/$(printf 'N%.0s' $(seq 300))" \
  put /dev/null "$url/branches/too-long/$(printf 'M%.0s' $(seq 1000))"
svnmucc "${U[@]}" -m "<script>alert('branch log')</script>" \
  mkdir "$url/branches/<svg onload=alert('branch')>" \
  mkdir "$url/tags/<img src=x onerror=alert('tag')>" \
  mkdir "$url/branches/externals" \
  propset svn:externals "$BEACON/<script>alert('ext')<%2Fscript> ext" "$url/branches/externals"

fx_pin_revprops "$repo"
head_rev="$(svnlook youngest "$repo")"
# Hostile authors (svn:author is only checked for UTF-8).
printf "<img src=x onerror=alert('author')>" > "$work/a"
svnadmin setrevprop "$repo" -r 2 svn:author "$work/a"
printf "<script>alert('author')</script>" > "$work/a"
svnadmin setrevprop "$repo" -r 3 svn:author "$work/a"

# Last revision from a dump stream: svn:log with invalid UTF-8, CR and control bytes, a file
# whose content and property are invalid UTF-8. `--bypass-prop-validation` is the only way in; repositories converted from other
# systems carry such values, and svn serves them as they are. Paths cannot be invalid UTF-8:
# the filesystem refuses them (E160005) even then.
next=$((head_rev + 1))
python3 - "$work/tail.dump" "$next" "$BEACON" <<'PY'
import sys
out, rev, beacon = sys.argv[1], int(sys.argv[2]), sys.argv[3].encode()
def props(d):
    b = b""
    for k, v in d.items():
        b += b"K %d\n%s\nV %d\n%s\n" % (len(k), k, len(v), v)
    return b + b"PROPS-END\n"
rp = props({
    b"svn:log": b"Invalid UTF-8 \xff\xfe\xc3\x28 and CR\r\nand NUL-free control \x07\x1b[2J <script>alert('bad log')</script>",
    # Not invalid UTF-8: mod_dav_svn writes svn:author raw into PROPFIND answers
    # (creator-displayname), so svn's own `ls -v` fails on such a repository.
    b"svn:author": b"<svg onload=alert('author r9')>",
    b"svn:date": b"2024-01-01T%02d:00:00.000000Z" % rev,
})
text = b"invalid UTF-8 content \xff\xfe\n<script>alert('content')</script>\n" + beacon + b"/content.png\n"
np = props({b"custom:bad-utf8": b"\xc0\xaf\xed\xa0\x80"})
s = b"SVN-fs-dump-format-version: 2\n\n"
s += b"Revision-number: %d\nProp-content-length: %d\nContent-length: %d\n\n%s\n" % (rev, len(rp), len(rp), rp)
s += (b"Node-path: trunk/bad-utf8.txt\nNode-kind: file\nNode-action: add\n"
      b"Prop-content-length: %d\nText-content-length: %d\nContent-length: %d\n\n%s%s\n\n"
      % (len(np), len(text), len(np) + len(text), np, text))
open(out, "wb").write(s)
PY
svnadmin load -q --bypass-prop-validation "$repo" < "$work/tail.dump"

echo "hostile repository at $repo (r$(svnlook youngest "$repo"), BEACON=$BEACON)"
echo "dump sha256 $(svnadmin dump -q "$repo" | sha256sum | cut -d' ' -f1)"
