# OpenSVN Design System — v1 (tạm chốt)

**Trạng thái:** Tạm chốt · **Ngày:** 2026-10-13 · **Nguồn sự thật hình ảnh:** [system-design/ui-mockup-main-window.html](system-design/ui-mockup-main-window.html) (bản v1)
Đặc tả màn hình/menu: [system-design/01-ui-screens-menus-and-interactions.md](system-design/01-ui-screens-menus-and-interactions.md)

Thay đổi token/component sau mốc này phải cập nhật **cả** file này và mockup, ghi vào §10.

## 1. Định hướng
- **Công cụ làm việc hằng ngày cho dev/artist**: dày thông tin nhưng dễ quét; trạng thái nổi bật hơn trang trí.
- Trung tính lạnh (xám ngả xanh) + **1 màu accent xanh dương** cho hành động chính và vùng chọn. Màu ngữ nghĩa (ok/warn/bad/info) tách biệt accent.
- Hai theme sáng/tối thiết kế song song, mặc định theo hệ thống.
- Hình dạng nhẹ: bo góc nhỏ, viền mảnh, bóng chỉ dùng cho lớp nổi (menu, popover, dialog).

## 2. Màu (design tokens)

### 2.1 Bảng token
| Token | Light | Dark | Dùng cho |
|---|---|---|---|
| `--bg` | `#eef1f5` | `#0f141b` | Nền ngoài cửa sổ / vùng trống |
| `--surface` | `#ffffff` | `#161d27` | Nền cửa sổ, pane, dialog |
| `--surface-2` | `#f6f8fb` | `#1b2430` | Menu bar, status bar, ô input, header cột, hover hàng |
| `--line` | `#dbe1ea` | `#2a3544` | Viền, phân cách |
| `--fg` | `#1b2533` | `#e3e9f2` | Chữ chính |
| `--muted` | `#5d6b7e` | `#93a1b5` | Chữ phụ, meta, số dòng |
| `--accent` | `#2363d1` | `#6ea0f5` | Nút chính, tab đang chọn, focus ring, link |
| `--accent-soft` | `#e3ecfb` | `#1d2d4a` | Chip/segment đang bật |
| `--sel` | `#dbe7fb` | `#22365a` | Hàng đang chọn, menu đang mở |
| `--ok` | `#1f8a4c` | `#4cc27e` | Trạng thái A, thành công, đã giải quyết |
| `--warn` | `#b26a00` | `#e6a23c` | Trạng thái M, conflict, cảnh báo |
| `--warn-soft` | `#fff4dc` | `#3a2d12` | Nền banner/khối cảnh báo, block conflict |
| `--bad` | `#c23b32` | `#f07067` | D, !, lock người khác, nút nguy hiểm |
| `--bad-soft` | `#fde8e6` | `#3d1c1a` | Nền khối cảnh báo nguy hiểm |
| `--info` | `#6a4bc4` | `#a58cf0` | R (move), hunk header, tag phase |
| `--add-bg` | `#e6f6ec` | `#16301f` | Dòng thêm trong diff, block đã chọn |
| `--del-bg` | `#fdeceb` | `#3a1b1a` | Dòng xóa trong diff |
| `--shadow` | `0 12px 40px rgba(20,34,58,.16)` | `0 12px 40px rgba(0,0,0,.5)` | Lớp nổi |
| `--on-accent`, `--on-bad`, `--on-warn` | `#ffffff` | `#ffffff` | Chữ trên nền đặc accent / danger / warn (tách token để sửa tương phản một chỗ; giá trị chờ PO quyết) |
| `--overlay` | `rgba(10,18,30,.35)` | `rgba(10,18,30,.35)` | Nền mờ sau dialog/sheet |

### 2.2 Quy tắc
- Component chỉ dùng token, không dùng mã màu trực tiếp. Chữ trên `--accent`/`--bad` là `#fff`.
- Không truyền đạt trạng thái **chỉ bằng màu**: luôn kèm ký hiệu (M/A/D…) hoặc chữ + tooltip.
- Tương phản tối thiểu WCAG AA (4.5:1 cho chữ thường) trên cả hai theme.

### 2.3 Map sang shadcn/ui (khi code)
| shadcn | OpenSVN |
|---|---|
| `--background` | `--surface` |
| `--foreground` | `--fg` |
| `--card`, `--popover` | `--surface` |
| `--muted` (nền) / `--muted-foreground` | `--surface-2` / `--muted` |
| `--primary` / `--primary-foreground` | `--accent` / `#fff` |
| `--accent` (nền hover) | `--sel` |
| `--destructive` | `--bad` |
| `--border`, `--input` | `--line` |
| `--ring` | `--accent` |
Triển khai: không khai báo biến alias shadcn (`--muted`/`--accent` trùng tên token OpenSVN nhưng khác nghĩa); khi vendor component shadcn thì dịch class (`bg-accent`→`bg-sel`, `bg-muted`→`bg-surface-2`, `bg-primary`→`bg-accent`).
Token ngữ nghĩa riêng (`--ok`, `--warn`, `--info`, `--add-bg`, `--del-bg`, `--*-soft`) giữ nguyên tên.

## 3. Chữ
| Vai trò | Font | Cỡ / weight | Ghi chú |
|---|---|---|---|
| UI | IBM Plex Sans → system-ui, -apple-system, "Segoe UI" | 13px / 400 | Cỡ gốc toàn app |
| Nhấn mạnh | IBM Plex Sans | 13px / 600 | Tên WC, branch, tab đang chọn |
| Phụ / meta | IBM Plex Sans | 11–12px / 400, `--muted` | Nhãn toolbar, meta revision |
| Eyebrow / nhóm | IBM Plex Sans | 10–11px / 500, UPPERCASE, letter-spacing .06–.07em | Nhãn nhóm trong list, header bảng |
| Tiêu đề dialog / sheet | IBM Plex Sans | 15px / 600 | |
| Code, diff, path, revision, fingerprint | IBM Plex Mono → ui-monospace, "SF Mono", Consolas | 12px / 400, line-height 1.6 | |
| Phím tắt | IBM Plex Mono | 11px, `--muted` | |
- Số trong cột/bảng/thời gian: `font-variant-numeric: tabular-nums`.
- Font đóng gói cùng app (không tải mạng). Người dùng đổi được font diff trong Settings.

## 4. Khoảng cách, bo góc, kích thước
- **Thang spacing (px):** 2 · 4 · 6 · 8 · 10 · 12 · 14 · 16 · 20 · 24. Dùng `gap` của flex/grid thay margin lẻ.
- **Padding chuẩn:** hàng list `5px 10px` · toolbar item `8px 14px` · header pane `7px 14px` · dialog body `14px 16px` · menu item `5px 9px`.
- **Bo góc:** 4 (badge, nút nhỏ trong block) · 6 (input, nút, hàng menu) · 8 (popover, dropdown, card tài khoản) · 10 (cửa sổ, dialog, sheet).
- **Viền:** 1px `--line`. Focus: `outline 2px --accent, offset 1px` cho mọi phần tử tương tác.
- **Kích thước cố định:** sidebar 330px mặc định (kéo 220–600) · cửa sổ tối thiểu 960×600 · dialog 480px · popover 320px · dropdown menu ≥ 270px · sheet cách mép cửa sổ 18px · Activity panel cao tối đa 150–200px.

## 5. Bố cục cửa sổ chính
```
Menu bar (surface-2)
Toolbar: [Working copy ▾][Branch ▾][Update] ··· [Activity]
Banner (tùy trạng thái)
┌ Sidebar 330px: Tabs · Filter · List · Commit box ┬ Main: Header pane · Nội dung (diff/chi tiết/bảng) ┐
Activity panel (ẩn/hiện)
Status bar (surface-2)
```
Lớp nổi theo thứ tự z: menu/popover (5–6) < context menu (8) < overlay dialog/sheet (9) < toast (10).

## 6. Component

| Component | Đặc tả | Trạng thái |
|---|---|---|
| **Button** | 13px, padding `6px 12px`, bo 6, viền `--line`, nền `--surface` | hover nền `--surface-2`; disabled opacity .45 |
| **Button primary** | nền + viền `--accent`, chữ trắng 600 | 1 nút primary / vùng |
| **Button danger** | nền `--bad`, chữ trắng 600 | Chỉ cho thao tác không hoàn tác (Discard, Steal lock) |
| **Toolbar item** | 2 dòng: nhãn nhỏ `--muted` 11px + giá trị 600; icon mono 18px; ngăn cách viền phải | hover `--surface-2`; Update có badge `↓N` (nền accent, bo 9) và icon xoay khi chạy (tắt khi reduced-motion) |
| **Tabs** | chia đều, chữ `--muted`; đang chọn chữ `--fg` 600 + gạch dưới 2px `--accent`; badge số đếm | |
| **Segmented control** | khung viền bo 7; mục bật nền `--accent-soft` chữ `--accent` | Split/Unified, macOS/Windows |
| **Chip lọc** | 11px, bo 10, viền `--line`; bật: nền `--accent-soft`, không viền | |
| **Input / select / textarea** | padding `6px 8px`, bo 6, nền `--surface-2`, viền `--line` | focus ring accent |
| **List row (file)** | grid `18px 20px 1fr auto`: checkbox · ký hiệu trạng thái · path (thư mục `--muted`, tên `--fg`) · badge lock | hover `--surface-2`, chọn `--sel` |
| **List row (revision)** | 2 dòng: message (1 dòng, ellipsis) + `rN · author · thời gian` 11px tabular | incoming: tiền tố "↓ chưa update" màu accent |
| **Status glyph** | mono 12px 500, căn giữa ô 20px; màu theo §7 | C: chữ trắng trên nền `--warn` |
| **Badge lock** | 11px; của bạn `--accent`, người khác `--bad` | |
| **Banner** | full width dưới toolbar, padding `7px 14px`, nền `--warn-soft` (cảnh báo) / `--bad-soft` (nguy hiểm); tiêu đề đậm màu ngữ nghĩa + mô tả + nút hành động bên phải | |
| **Dropdown menu** | nền `--surface`, viền, bo 8, `--shadow`, padding 5; item `5px 9px` bo 5, nhãn trái + phím tắt phải; ngăn cách `hr` | hover nền `--accent` chữ trắng; disabled `--muted` |
| **Popover (WC/Branch)** | 320px; ô tìm kiếm đầu; nhóm có eyebrow; hàng hiện tại 600; thanh hành động cuối có viền trên | |
| **Dialog** | 480px (≤ 92%), bo 10; header 15px/600 có viền dưới; body gap 10; footer nút phải (Cancel trước, primary/danger sau) | overlay `rgba(10,18,30,.35)`; Esc / click nền đóng; focus vào input đầu |
| **Khối thông tin trong dialog** | `warnbox` (nền `--warn-soft`), `badbox` (nền `--bad-soft`), bo 6, padding `8px 10px` | |
| **Sheet (cửa sổ lớn)** | Conflict resolver, Settings: inset 18px, grid header / body (nav 230px + nội dung) / footer `--surface-2` | nav item chọn: `--sel` + 600 |
| **Diff line** | grid `44px 44px 18px 1fr`, mono 12/1.6; thêm `--add-bg`, xóa `--del-bg`, hunk `--info` trên `--surface-2` | |
| **Conflict block** | viền trên/dưới `--warn`, nền `--warn-soft`; đã chọn: viền `--ok`, nền `--add-bg`; nút nhỏ 11px trong block | |
| **Activity row** | grid `20px 1fr 160px 70px`: icon · mô tả · progress bar 6px (track `--line`, fill `--accent`) · % | lỗi: chữ `--bad`, icon ✕ |
| **Toast** | góc dưới phải, nền `--fg` chữ `--surface`, bo 7, tự ẩn 2.6–5s | |
| **Status bar** | 12px `--muted`, nền `--surface-2`, các mục cách 16px, 1 dòng | mục kết nối dùng `--ok`/`--bad` |
| **Empty state** | căn giữa pane, tiêu đề đậm + 1 câu giải thích + tối đa 2–3 nút hành động | |

## 7. Ký hiệu trạng thái SVN
| Glyph | Token | Ý nghĩa |
|---|---|---|
| M | `--warn` | Đã sửa |
| A | `--ok` | Thêm mới |
| D | `--bad` | Xóa |
| R | `--info` | Thay thế / move |
| ? | `--muted` | Chưa quản lý (không tự tick khi commit) |
| ! | `--bad` | Mất file |
| C | trắng trên `--warn` | Conflict (chặn commit file đó) |
| 🔒 | `--accent` (bạn) / `--bad` (người khác) | Lock |
| ↓ | `--accent` | Có revision mới trên server |
| ⑂ / 🏷 | `--muted` | Branch / tag (chỉ khi repo có layout trunk/branches/tags) |
| ▤ | `--muted` | Thư mục (chế độ Location, repo không có layout) |
| ⧉ | `--info` | Revision có `svn copy` |
Icon UI dùng bộ **lucide** (stroke 1.5–2px, 16px trong list, 18px trong toolbar); glyph trạng thái giữ dạng chữ mono để quét nhanh.

## 8. Tương tác & chuyển động
- Thao tác dài luôn hiển thị trong Activity + có Cancel; không chặn UI.
- Thao tác không hoàn tác: dialog xác nhận với nút danger; Steal lock cần tick xác nhận trước khi nút bật.
- Phản hồi sau hành động: toast mô tả kết quả cụ thể ("Đã commit 3 file · r48215").
- Chuyển động tối thiểu: chỉ icon Update xoay và progress bar; tôn trọng `prefers-reduced-motion`.
- Mọi chức năng đi tới được bằng bàn phím; phím tắt theo [01 §8](system-design/01-ui-screens-menus-and-interactions.md#8-menu-bar).

## 9. Quy ước nền tảng & nội dung
- **macOS:** menu bar hệ thống, menu ứng dụng `OpenSVN` (About/Settings/Quit), menu Window; phím `⌘ ⇧ ⌥`; "Show in Finder"; mật khẩu "Keychain".
- **Windows/Linux:** menu trong cửa sổ, Settings ở File, About ở Help; phím `Ctrl+ Shift+ Alt+`; "Show in Explorer" / "Show in file manager"; "Windows Credential Manager"; nút SSO "Đăng nhập bằng tài khoản Windows".
- **Ngôn ngữ:** tiếng Việt mặc định, thuật ngữ SVN giữ tiếng Anh khi là tên lệnh quen thuộc (Commit, Update, Branch, Lock). Câu chủ động, nói rõ hậu quả ("Không thể hoàn tác"). Lỗi nói nguyên nhân + cách sửa, không xin lỗi chung chung.
- **Dữ liệu mẫu** trong mockup là ví dụ; không dùng tên server/người thật trong tài liệu công khai.

## 10. Lịch sử thay đổi
| Ngày | Phiên bản | Thay đổi |
|---|---|---|
| 2026-10-13 | v1.1 | Thêm token `--on-accent/--on-bad/--on-warn`, `--overlay` (giá trị không đổi); ghi chú dịch class shadcn |
| 2026-10-13 | v1 (tạm chốt) | Token màu 2 theme, chữ IBM Plex, component cửa sổ chính, dialog, sheet Conflict resolver & Settings, glyph trạng thái, quy ước nền tảng; toolbar Branch ▾ / Location ▾ hai chế độ (SVN không có branch/tag thật) |

## Câu hỏi còn mở
- Logo/biểu tượng app chưa có.
- Link mở revision trên VisualSVN web chưa đối chiếu (mẫu `#{repo}/commit/r{rev}/`).
- Chưa kiểm định tương phản tự động cho toàn bộ cặp token (cần chạy công cụ kiểm khi dựng app).
