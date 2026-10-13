# Phase 4 — Desktop app MVP

## Mục tiêu
Giao diện kiểu GitHub Desktop trên Windows/Linux/macOS. FE làm song song từ phase 2 (repo browser, log).

## Màn hình
- **Sidebar**: danh sách working copy (thêm WC có sẵn — kể cả WC tạo bởi TortoiseSVN — hoặc checkout mới).
- **Toolbar**: Current branch (dropdown) · Check updates · Update · Commit.
- **Changes**: file + checkbox, diff (CodeMirror 6 merge view, image compare), message, Commit. Tự add file mới, delete file mất, gợi ý move khi nhận ra rename. Discard = revert. Ignore. Changelist (Change Sets).
- **History**: log phân trang, changed paths, diff; "Revert this revision".
- **Branches**: cấu hình trunk/branches/tags, cây branch/tag, Switch, Create branch.
- **Repo browser**: duyệt theo revision, blame, lock/unlock, tải file.
- **Conflict**: Mine/Theirs/mở editor ngoài → resolved.
- **Activity panel**: tiến trình, lỗi dễ hiểu kèm hành động, Cancel.
- Đăng nhập: SSO (Windows domain) / tài khoản AD; quản lý credential.

## Kỹ thuật
- Tauri command/event; thao tác dài chạy nền, stream progress.
- Hàng đợi thao tác theo WC.
- FS watcher (`notify`): ReadDirectoryChangesW / FSEvents / inotify (cảnh báo giới hạn `max_user_watches` trên Linux).
- Menu native, phím tắt theo OS (Ctrl vs Cmd), tray/notification.
- Đóng gói + ký: MSI/NSIS (Authenticode), DMG (Developer ID + notarize), AppImage/deb/rpm; auto-update.

## Tiêu chí xong
- Dev nội bộ dùng thay SmartSVN/TortoiseSVN 2 tuần cho luồng hằng ngày, không lỗi chặn, không mất dữ liệu.
