#!/usr/bin/env bash
# Deterministic repo WITHOUT trunk/branches/tags: Vietnamese folder names (NFC) and spaces.
# Usage: make-nolayout.sh <repo-path>
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
. "$here/lib.sh"

repo="${1:?usage: make-nolayout.sh <repo-path>}"
fx_create "$repo" "6d9f2c1e-0b1a-4c5e-9a31-0a5f2e1d7b02"
url="file://$repo"
wc="$(mktemp -d)"
trap 'rm -rf "$wc"' EXIT
svn checkout -q "$url" "$wc"
cd "$wc"

mkdir -p "Tài liệu/Báo cáo quý 1" "Hình ảnh" "folder with spaces/sub folder"
printf 'Kế hoạch năm 2024\nXin chào\n' > "Tài liệu/Kế hoạch năm.txt"
printf 'Báo cáo doanh thu\n' > "Tài liệu/Báo cáo quý 1/doanh thu.txt"
printf 'file with spaces\n' > "folder with spaces/sub folder/a file.txt"
fx_random_bytes "nolayout/photo" 8192 > "Hình ảnh/ảnh đại diện.jpg"
printf 'root level file\n' > "Đọc tôi.txt"
svn add -q --force .
svn propset -q svn:mime-type image/jpeg "Hình ảnh/ảnh đại diện.jpg"
fx_commit . "Thêm tài liệu ban đầu"

printf 'Cập nhật kế hoạch\n' >> "Tài liệu/Kế hoạch năm.txt"
fx_commit . "Cập nhật kế hoạch"

svn mv -q "Đọc tôi.txt" "Tài liệu/Đọc tôi.txt"
fx_commit . "Di chuyển tệp"

cd /
fx_pin_revprops "$repo"
fx_check_hash "$repo" nolayout
