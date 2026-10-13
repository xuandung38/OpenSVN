# Phase 5 — Repo lớn & binary

## Mục tiêu
Nhanh hơn svn CLI trên repo lớn mà **không đổi định dạng WC** (ADR-004).

## Tối ưu
- **Format 32 + pristines on demand** (`store-pristine=no`): tùy chọn khi checkout; hydrate pristine khi diff/revert; commit gửi fulltext. Cảnh báo: tool khác phải ≥1.15.
- **Tải song song** nhiều kết nối khi checkout/update; streaming ghi file + pristine cùng lúc; resume file lớn bị đứt.
- **Status tăng dần**: cache stat trong RAM + FS watcher; chỉ quét lại phần thay đổi.
- **Remote status định kỳ** chạy nền, giới hạn tần suất.
- **Sparse checkout UI**: cây từ server có checkbox, ước tính dung lượng trước khi tải.
- **Cache log** SQLite trong thư mục dữ liệu app (không ghi vào `.svn/`).

## Lock UX
- Hiển thị chủ lock, thời gian, comment; Lock/Unlock/Steal (xác nhận).
- File `svn:needs-lock`: read-only, nút "Lock để sửa".
- Cảnh báo khi sửa binary đang bị người khác lock.
- Gợi ý auto-props `svn:needs-lock` cho psd, fbx, xlsx, ...

## Theo dõi thay đổi
- Kiểu "Transactions" của SmartSVN: badge revision mới + thông báo hệ thống.

## Kiểm thử
- Benchmark ≥100k file / ≥50GB binary: so thời gian checkout/update/status và dung lượng đĩa với svn CLI 1.15.
- Compat: WC format 32 do engine tạo dùng được bằng svn 1.15.

## Tiêu chí xong
- Checkout nhanh hơn svn CLI ≥30%; status < 3s; format 32 giảm rõ dung lượng với repo nhiều binary.
