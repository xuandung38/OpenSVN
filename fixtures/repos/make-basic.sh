#!/usr/bin/env bash
# Deterministic "standard layout" fixture repo (trunk/branches/tags).
# Usage: make-basic.sh <repo-path>      (needs svn, svnadmin, svnlook, openssl, sha256sum)
# Covers: LF/CRLF/mixed text, seeded binary, svn:eol-style, svn:keywords, svn:needs-lock,
# svn:mime-type, svn:executable, svn:ignore, svn:externals, rename and copy history,
# a branch and a tag,
# and a node with 8 properties.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
. "$here/lib.sh"

repo="${1:?usage: make-basic.sh <repo-path>}"
fx_create "$repo" "6d9f2c1e-0b1a-4c5e-9a31-0a5f2e1d7b01"
url="file://$repo"
wc="$(mktemp -d)"
trap 'rm -rf "$wc"' EXIT
svn checkout -q "$url" "$wc"
cd "$wc"

# r1: layout
svn mkdir -q trunk branches tags
fx_commit . "Create standard layout"

# r2: content (LF, CRLF, mixed EOL, source, seeded binaries)
svn mkdir -q trunk/src trunk/docs trunk/data trunk/with-externals
printf 'Hello OpenSVN\nsecond line\nthird line\n' > trunk/README.txt
printf 'windows\r\nline endings\r\nhere\r\n' > trunk/crlf.txt
printf 'mixed\nlf and\r\ncrlf lines\nend\r\n' > trunk/mixed-eol.txt
printf 'int main(void) {\n    return 0;\n}\n' > trunk/src/main.c
printf '# Docs\n\nSome documentation.\n' > trunk/docs/index.md
printf 'must be locked before editing\n' > trunk/locked.txt
printf '#!/bin/sh\necho run\n' > trunk/run.sh
# shellcheck disable=SC2016
printf '$Id$\n$Rev$\nkeyword file\n' > trunk/keywords.txt
fx_random_bytes "basic/blob" 65536 > trunk/data/blob.bin
fx_random_bytes "basic/image" 4096 > trunk/data/image.png
svn add -q trunk/README.txt trunk/crlf.txt trunk/mixed-eol.txt trunk/src/main.c \
  trunk/docs/index.md trunk/locked.txt trunk/run.sh trunk/keywords.txt \
  trunk/data/blob.bin trunk/data/image.png
fx_commit . "Add text, source and binary content"

# r3: properties
svn propset -q svn:eol-style native trunk/README.txt
svn propset -q svn:eol-style CRLF trunk/crlf.txt
svn propset -q svn:keywords "Id Rev" trunk/keywords.txt
svn propset -q svn:needs-lock '*' trunk/locked.txt
svn propset -q svn:executable '*' trunk/run.sh
svn propset -q svn:mime-type image/png trunk/data/image.png
svn propset -q svn:mime-type application/octet-stream trunk/data/blob.bin
svn propset -q svn:ignore $'*.o\nbuild\n*.tmp' trunk
fx_commit . "Set properties"

# r4: externals (relative to the repo root, so no external host is involved)
svn propset -q svn:externals $'^/trunk/docs docs-ext\n^/trunk/README.txt readme-ext.txt' trunk/with-externals
fx_commit . "Add externals"

# r5: rename, r6: copy within trunk
svn update -q
svn mv -q trunk/src/main.c trunk/src/app.c
fx_commit . "Rename main.c to app.c"
svn cp -q trunk/src/app.c trunk/src/app-copy.c
fx_commit . "Copy app.c"

# r7: branch, r8: branch change, r9: tag
svn update -q
svn cp -q trunk branches/feature-x
fx_commit . "Create branch feature-x"
printf 'feature work\n' >> branches/feature-x/README.txt
fx_commit . "Change on feature-x"
svn update -q
svn cp -q trunk tags/v1.0
fx_commit . "Tag v1.0"

# r10: one node with 8 properties. svn serialises a property list in per-process hash order,
# so this node is what proves wc.db dumps are compared independently of that order.
svn update -q
printf 'many properties\n' > trunk/many-props.txt
svn add -q trunk/many-props.txt
svn propset -q svn:eol-style native trunk/many-props.txt
svn propset -q svn:keywords "Id Rev Date" trunk/many-props.txt
svn propset -q svn:mime-type text/plain trunk/many-props.txt
svn propset -q svn:needs-lock '*' trunk/many-props.txt
svn propset -q custom:alpha one trunk/many-props.txt
svn propset -q custom:beta two trunk/many-props.txt
svn propset -q custom:gamma three trunk/many-props.txt
svn propset -q custom:delta four trunk/many-props.txt
fx_commit . "Add node with many properties"

cd /
fx_pin_revprops "$repo"
fx_check_hash "$repo" basic
