# Phase 6 — Nâng cao: merge, conflict, revision graph, tích hợp OS

## Mục tiêu
Đạt các tính năng khác biệt của SmartSVN.

## Yêu cầu
- **Merge**: merge range / cherry-pick revision / sync từ trunk; đọc-ghi `svn:mergeinfo`, REPORT `mergeinfo-report`; preview (dry-run) trước khi áp.
- **Conflict solver 3-way**: base/mine/theirs, chọn từng block hoặc sửa tự do, đánh dấu resolved.
- **Tree conflict**: sửa local vs xóa/move trên server, và ngược lại; UI giải thích + lựa chọn.
- **Switch nâng cao**, mixed-revision cảnh báo.
- **Externals** (`svn:externals`): checkout/update/commit lồng nhau, ghim revision.
- **Properties editor** dạng text + diff.
- **Revision Graph**: dựng từ log `copyfrom` + mergeinfo, cache SQLite; hiển thị merged/unmerged.
- **Shelve**: lưu thay đổi local thành patch + metadata trong thư mục dữ liệu app (không ghi vào `.svn/`), khôi phục được.
- **Hoàn thiện Negotiate**: xử lý ticket hết hạn, nhiều realm; Explorer/Finder integration (cân nhắc xung đột overlay icon với TortoiseSVN).

## Kiểm thử
- Differential cho merge + mergeinfo so với svn CLI.
- Kịch bản tree conflict chuẩn (theo bộ test `merge_tree_conflict_tests` của Subversion làm tham chiếu).

## Tiêu chí xong
- Merge branch ↔ trunk trên repo nội bộ cho kết quả và mergeinfo giống svn CLI.
