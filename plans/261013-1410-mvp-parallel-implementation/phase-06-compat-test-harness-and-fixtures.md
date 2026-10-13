---
phase: 6
title: "Compat test harness and fixtures"
status: pending
priority: P1
effort: "2–3 tuần · 1 agent (sau đó bảo trì theo yêu cầu các phase)"
dependencies: [1]   # cần gói 01a
wave: 1
owner_paths: ["docker/", "fixtures/", "xtask/", "tests/compat/{src,Cargo.toml,README.md}", "tests/compat/scenarios/harness/", ".github/workflows/compat.yml"]
---

# Phase 6: Compat test harness and fixtures

## Overview
Hạ tầng kiểm chứng "giống svn thật" cho mọi phase: server SVN trong Docker, repo mẫu, bắt gói tin giao thức, sinh working copy bằng svn 1.14/1.15, và runner so sánh kết quả engine với svn CLI. Đây là xương sống chất lượng của dự án.

## Requirements
- Functional:
  - `docker/svn-server/`: Apache httpd 2.4 + mod_dav_svn (**pin Subversion 1.14.5 và 1.15.0** — tarball có checksum, build từ source nếu distro chưa có; 2 image) + HTTPS (CA test tự sinh) + Basic auth (htpasswd) + `mod_dumpio`/log để bắt request; cấu hình gần VisualSVN (SVNParentPath, `SVNAdvertiseV2Protocol On`, `SVNAutoversioning off`, `SVNAllowBulkUpdates Prefer`).
  - `docker/svn-client/`: image svn CLI 1.14.5 và 1.15.0 để chạy lệnh tham chiếu.
  - **Xác minh môi trường thật (tuần 1)**: phiên bản mod_dav_svn của VisualSVN nội bộ, TortoiseSVN đang dùng → ghi vào `tests/compat/README.md`; oracle chính = 1.14.5 nếu VisualSVN/TortoiseSVN chưa lên 1.15.
  - **Fixture độc hại**: `docker/evil-server/` (server giả nhỏ trả update-report/PROPFIND/log có path `../x`, `.svn/wc.db`, `CON.txt`, `a:b` (ADS), symlink `dir -> /tmp` rồi file trong `dir/`, href sang host khác/IP nội bộ, redirect khác origin) + repo có commit message/tên file/prop chứa `<script>`, `<img onerror>`, SVG có script.
  - `fixtures/repos/`: script tạo repo mẫu xác định (deterministic): text nhiều EOL, binary lớn (tạo ngẫu nhiên seed cố định), props (`svn:eol-style`, `svn:keywords`, `svn:needs-lock`, `svn:mime-type`, `svn:executable`, `svn:ignore`, `svn:externals`), rename/copy, branch/tag theo layout chuẩn, **repo không có layout** (thư mục tiếng Việt có dấu, khoảng trắng), 1 repo 100k file cho perf (sinh khi cần, không commit).
  - `fixtures/protocol/`: request/response thật đã bắt (OPTIONS, PROPFIND, log, update-report, commit đầy đủ, lock) — đã ẩn header auth, host đổi thành `svn.test.local`.
  - `fixtures/wc/`: working copy sinh bởi svn 1.14 (format 31) và 1.15 (format 31/32) ở nhiều trạng thái (sạch, sửa, add, delete, move, conflict text/tree, lock, sparse, externals, mixed-rev) → nén tar + script tái tạo.
  - `fixtures/svndiff/`, `fixtures/diff/`: dữ liệu golden cho P02/P05.
  - `tests/compat/`: crate test runner — DSL kịch bản (`step: engine | svn14 | svn15 | tortoise-manual`), so sánh: cây file (nội dung, quyền exec), `svn status --xml`, `svn info --xml`, dump `wc.db` (chuẩn hóa: bỏ cột thời gian, sắp xếp).
  - `cargo xtask compat up|down|capture <scenario>|fixtures` .
- Non-functional: bật server < 30s; toàn bộ compat suite < 20 phút trên CI Linux; chạy được local trên macOS (Docker Desktop/OrbStack).

## Architecture
```
docker/svn-server/{Dockerfile.1.14,Dockerfile.1.15,httpd.conf,dav_svn.conf,gen-certs.sh}
docker/svn-client/{Dockerfile.1.14,Dockerfile.1.15}
docker/compose.yml
fixtures/repos/{make-basic.sh,make-nolayout.sh,make-large.sh}
fixtures/protocol/<op>/<n>-{request,response}.http
fixtures/wc/<svnver>/<state>.tar.zst + manifest.json
tests/compat/src/{lib.rs,scenario.rs,runner.rs,compare.rs,wcdb_dump.rs}
tests/compat/scenarios/*.yaml
xtask/src/compat.rs
.github/workflows/compat.yml   (ubuntu, services docker; chạy khi PR đụng crates/svn-protocol|svn-ra|svn-wc|svn-client)
```

## Related Code Files
- Create: như trên. Không sửa crate engine.

## Implementation Steps
1. Dockerfile server 1.14/1.15 (build từ source hoặc package distro có đúng version), HTTPS + Basic; healthcheck.
2. Script repo mẫu deterministic (seed cố định), kiểm hash nội dung repo sau tạo.
3. Bắt gói tin: cấu hình svn client đi qua `mitmproxy` (container) hoặc `mod_dumpio`; script chuẩn hóa/ẩn danh → `fixtures/protocol/`.
4. Sinh fixture WC cho từng trạng thái bằng svn 1.14/1.15 trong container; manifest mô tả trạng thái.
5. Runner compat + DSL; so sánh `svn status/info --xml`, cây file, dump wc.db chuẩn hóa.
6. Kịch bản mẫu chạy được ngay với "engine = svn15" (tự so svn với svn) để kiểm harness trước khi có engine.
7. Workflow `compat.yml` + cache image (ghcr.io private của repo).
8. Tài liệu `tests/compat/README.md`: cách thêm kịch bản, cách bắt fixture mới. Thư mục kịch bản theo owner: `protocol/` (P03), `wc/` (07A/B), `ra/read/` (10a), `ra/write/` (10b), `client/` (11U/C), `e2e/` (12b, 15). E2E Windows: dùng VisualSVN test nội bộ hoặc server Linux qua mạng (runner Windows hosted không chạy Docker Linux).
9. Evil server + hostile repo fixtures (dùng bởi 03, 07, 10, 12, 13).

## Success Criteria
- [ ] `cargo xtask compat up` chạy server 1.14 & 1.15 HTTPS trên Linux CI và macOS local.
- [ ] Bộ fixture protocol phủ: OPTIONS, PROPFIND (depth 0/1), GET, log (có/không changed paths), update-report (checkout, update, switch, status -u), commit (add/modify/delete/copy/prop, có lock token), LOCK/UNLOCK, get-locks, file-revs.
- [ ] ≥ 25 trạng thái WC fixture cho mỗi phiên bản svn.
- [ ] Runner tự so "svn15 vs svn15" xanh cho mọi kịch bản mẫu (harness đúng).
- [ ] Fixture không chứa host/tài khoản thật (CI kiểm grep).
- [ ] Evil server + hostile repo có sẵn và có README mô tả từng ca tấn công.

## Risk Assessment
- Package svn 1.15 chưa có trên distro → build từ source trong Dockerfile (cache layer).
- Fixture lớn làm repo nặng → nén zstd, repo 100k file sinh lúc chạy, không commit.
- Nhiều phase cùng thêm kịch bản → thư mục con theo phase owner.
