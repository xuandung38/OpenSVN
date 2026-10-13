# Phase 3 — Working copy engine tương thích wc.db

## Mục tiêu
`svn-wc` + `svn-client` đọc/ghi `.svn/` **đúng chuẩn libsvn_wc** (ADR-004): checkout/update/status/commit/revert an toàn, dùng chung với svn CLI/TortoiseSVN.

## Đặc tả cần cài (tham chiếu source Subversion, Apache 2.0)
- `libsvn_wc/wc-metadata.sql`, `wc-queries.sql`: schema format 31 (+32).
- Bảng chính: `REPOSITORY`, `WCROOT`, `NODES` (op_depth: base/working layers), `ACTUAL_NODE` (props/changelist/conflict local), `PRISTINE` (SHA-1, MD5, refcount), `LOCK`, `WC_LOCK`, `WORK_QUEUE`, `EXTERNALS`.
- Pristine: `.svn/pristine/xx/<sha1>.svn-base`; format 32: pristine tùy chọn (hydrate khi cần).
- `WORK_QUEUE` items (file-install, file-remove, file-move, sync-file-flags, prej-install...) — cài đủ để vừa ghi vừa thực thi item do tool khác để lại.
- Conflict skel (text/prop/tree) đúng định dạng serialize của svn.
- `wcprops`/DAV cache (`dav_cache` trong NODES) cho HTTPv2.
- Kết quả: viết `docs/wc-db-format-notes.md`.

## Yêu cầu
- **Checkout / update / switch**: REPORT `update-report` (send-all, depth, sparse) → delta editor → file tạm → work queue → rename atomic; tải song song.
- **Status**: local (so stat với `translated_size`/`last_mod_time` trong NODES, chỉ hash khi khác) + remote (`status -u`).
- **Commit**: path được chọn → txn HTTPv2 → MERGE → cập nhật NODES/PRISTINE; gửi lock token; release lock.
- **add / delete / move / copy / revert / cleanup / changelist / upgrade** (không upgrade format cũ <31: yêu cầu svn CLI).
- **Translate**: `svn:eol-style`, `svn:keywords`, `svn:executable`, `svn:special` (symlink; Windows: file text như svn), `svn:needs-lock`.
- **Ignore**: `svn:ignore`, `svn:global-ignores`, `global-ignores` trong config svn của user.
- **Conflict text cơ bản**: diff3 → `.mine/.rN` + marker như svn.
- **Depth**: empty/files/immediates/infinity, `set-depth`, exclude.
- **Windows**: long path, không phân biệt hoa/thường, retry rename khi file bị giữ, CRLF.
- **Đồng thời**: lấy `WC_LOCK` như svn; SQLite busy-timeout; không giữ transaction dài.

## Kiểm thử
- **Compat hai chiều** (mỗi PR, CI): engine thao tác → svn 1.14/1.15 `status`/`info`/`update`/`commit` trên cùng WC không lỗi, kết quả khớp; svn thao tác → engine đọc đúng.
- So sánh dump `wc.db` (bỏ cột thời gian) giữa engine và svn sau cùng kịch bản.
- Crash test: kill ngẫu nhiên giữa update/commit → mở lại chạy work queue → `svn status` sạch, không mất thay đổi.
- proptest cho translate eol/keywords.
- Thử thủ công TortoiseSVN trên Windows.

## Tiêu chí xong
- Luồng checkout → sửa → commit → update (merge tự động) đúng trên repo nội bộ; TortoiseSVN mở WC không báo lỗi.
