---
phase: 12
title: "Frontend feature screens wired to IPC"
status: pending
priority: P1
effort: "12a: 3 tuần (wave 2) · 12b: 5–6 tuần (wave 2–3, trên FakeEngine trước) · 2 agent"
dependencies: [8, 9]
wave: 2-3
owner_paths:
  12a: ["apps/opensvn/src/features/{welcome,auth,settings,browse,history,checkout,sparse}/"]
  12b: ["apps/opensvn/src/features/{changes,diff,branch,location,dialogs,conflicts,banners,activity,menus,notifications}/", "tests/compat/scenarios/e2e/ui/"]
---

# Phase 12: Frontend feature screens wired to IPC

## Overview
Dựng màn hình, dialog, menu, luồng theo [doc 01](../../docs/system-design/01-ui-screens-menus-and-interactions.md) và [mockup v1](../../docs/system-design/ui-mockup-main-window.html) trên component P09 + IPC. Chạy trên mock adapter → FakeEngine (P08) → RealEngine (P15).
- **12a — remote** (mốc M1): Welcome, Sign in, Trust certificate, Settings/Accounts, Browse, History, blame, Checkout, Choose folders.
- **12b — working copy**: bắt đầu wave 2 trên FakeEngine, nối RealEngine khi 15b xong.

## Requirements
### 12a
- Welcome; **Sign in** (SSO Windows "Đăng nhập bằng tài khoản Windows" / tài khoản AD; hiện origin đầy đủ host:port, không chỉ realm); **Trust certificate**: chỉ hiện "Tin vĩnh viễn" khi lỗi là issuer không tin cậy/tự ký; sai hostname/hết hạn/thu hồi → chặn, chỉ có "Đóng"; cảnh báo riêng khi cert của host đã pin bị đổi.
- Settings 7 trang (doc 01 §9), gồm mẫu URL VisualSVN web; setting chạy lệnh (editor/diff/merge tool ngoài) chỉ đổi qua dialog chọn file native + xác nhận.
- Browse (cây repo theo revision, bảng, preview text, blame, log path, thao tác trực tiếp server: new folder, upload, rename, delete, copy — qua dialog có message).
- History (log phân trang, filter, chi tiết, changed paths, diff revision, so 2 revision).
- Checkout dialog (**MVP chỉ format 31** — ẩn lựa chọn format 32), Choose folders.
### 12b
- Changes (list ảo, checkbox tristate, nhóm changelist, chip lọc, file `?` không tự tick — tick thì tự add lúc commit, gợi ý move, commit box + lịch sử message, `Mod+Enter`), DiffView file/ảnh (side-by-side, onion, swipe)/binary/prop.
- Update button đa trạng thái + Update to revision; Branch ▾ / Location ▾ hai chế độ (doc 01 §3.9); Switch / New branch / New tag / Copy to new location.
- Dialogs: Discard, Lock/Steal (tick xác nhận), Ignore, Properties, Cleanup (gồm "gỡ lock của chương trình khác" có xác nhận), Update to revision. (Upgrade format: **ngoài MVP**.)
- Conflict resolver MVP: Mine/Theirs/mở editor ngoài/Mark resolved (3-way theo block nếu kịp, không chặn MVP).
- Banners (conflict, out-of-date, lock bị lấy, mixed-rev, WC bận bởi chương trình khác, tag), Activity, Status bar, toast, notification OS.
- Menu bar + Selection menu nhận `menu://<id>` (P08), bật/tắt theo ngữ cảnh; phím tắt doc 01 §8; context menu 5 loại (§6).
### Chung
- **Nội dung repo không bao giờ render HTML**: commit message, tên file, giá trị prop, `post-commit-err`, nội dung diff hiển thị dạng text (React escape mặc định); cấm `dangerouslySetInnerHTML` (lint P09); preview SVG/ảnh chỉ qua `<img src="asset/blob">`, không iframe/object; link trong message chỉ nhận `https?://` và mở bằng trình duyệt ngoài.
- Lỗi theo `AppError.actions` (Retry, Update, Cleanup, Sign in, Trust, Mở dialog lock, Mở settings).
- Non-functional: UI không chờ I/O; Changes cập nhật < 1s sau sự kiện; a11y AA, bàn phím đầy đủ.

## Architecture
```
apps/opensvn/src/features/
  12a: welcome/ auth/ settings/ browse/ history/ checkout/ sparse/
  12b: changes/ diff/ branch/ location/ dialogs/ conflicts/ banners/ activity/ menus/ notifications/
```
Mỗi feature: `*.tsx` + `use*.ts` (hook từ `ipc/queries`) + test Vitest (mock adapter).

## Related Code Files
- Create: như trên. Không sửa `components/`, `ipc/` (P09). Cần component/dep mới → PR nhỏ tới P09/lead.

## Implementation Steps
**12a**: 1) Welcome, Sign in, Trust cert, Settings, Accounts → 2) Browse, History, blame → 3) Checkout, Choose folders → 4) nối RealEngine remote (15a) → **M1**.
**12b**: 1) Changes + commit + diff local trên FakeEngine → 2) Update button + banner + Activity → 3) Branch/Location + switch/branch/tag/copy → 4) dialogs + Conflict resolver MVP + context/Selection menu → 5) nối RealEngine (15b) → 6) E2E luồng 10.1–10.5 (`tauri-driver`, Linux CI với server Docker P06; Windows dùng VisualSVN test nội bộ hoặc server Linux qua mạng).

## Success Criteria
- [ ] 12a: M1 chạy với VisualSVN thật trên 3 OS.
- [ ] Mọi màn hình/dialog sitemap doc 01 §2 (trừ Merge, Upgrade format) hoạt động với RealEngine.
- [ ] E2E luồng 10.1–10.5 xanh trên Linux CI; Windows/macOS theo checklist.
- [ ] Fixture "repo độc hại" (P06: message/tên file chứa `<script>`, `<img onerror>`, SVG có script) hiển thị an toàn — test tự động.
- [ ] Mọi mục menu/phím tắt doc 01 §8 hoạt động, bật/tắt đúng ngữ cảnh; lead review khớp mockup v1.

## Risk Assessment
- Engine thật khác fake → test feature chạy cả mock và RealEngine (job riêng).
- 12b lớn → ưu tiên luồng hằng ngày (Changes/commit/update) trước, dialog phụ sau.
