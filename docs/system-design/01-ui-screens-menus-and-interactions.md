# 01 — Giao diện, menu & tương tác

Cập nhật: 2026-10-13 · Phạm vi: Windows, Linux, macOS · Liên quan: [02](02-architecture-data-flow-and-reliability.md), [03](03-ipc-api-contract-and-data-model.md)

## 1. Nguyên tắc thiết kế
1. **Một cửa sổ chính, 3 tab** như GitHub Desktop: Changes · History · Browse. Mọi thao tác hằng ngày ≤ 2 click.
2. **Khái niệm SVN hiển thị bằng ngôn ngữ đơn giản**: "Update" thay "svn update", "Discard changes" thay "revert", "Lock để sửa".
3. **Thao tác nguy hiểm luôn xác nhận** (Discard, Steal lock, Delete trên server, Revert revision) và có mô tả hậu quả.
4. **Không chặn UI**: mọi lệnh mạng/đĩa chạy nền, hiện tiến trình trong Activity, hủy được.
5. **Tôn trọng quy ước OS**: macOS menu bar + Cmd; Windows/Linux menu trong cửa sổ + Ctrl. Ký hiệu `Mod` = Cmd (macOS) / Ctrl (Win/Linux).
6. **Bàn phím đầy đủ + accessibility**: mọi chức năng có phím tắt hoặc đi tới được bằng Tab; nhãn ARIA; tương phản AA; dark/light/theo hệ thống.

**Đã chốt với PO (2026-10-13):** có menu **Selection** riêng; file unversioned (`?`) **không tự tick**, tick thì tự `add` lúc commit; có tích hợp mở revision/file trên VisualSVN web.

Mockup tương tác: [ui-mockup-main-window.html](ui-mockup-main-window.html) — cửa sổ chính, Conflict resolver, Settings, Sign in, Trust certificate, Choose folders.

## 2. Sơ đồ màn hình (sitemap)
```
App
├── Welcome (chưa có WC)
│   ├── Checkout repository…
│   ├── Add existing working copy…
│   └── Recent URLs / bookmarks
├── Main window
│   ├── Toolbar: Working copy ▾ · Location ▾ (Branch ▾ khi repo có layout) · Update/Check · Activity
│   ├── Tab Changes   (danh sách thay đổi + commit box | diff)
│   ├── Tab History   (danh sách revision | chi tiết + diff)
│   ├── Tab Browse    (cây repo theo revision | preview/blame/log)
│   ├── Banner        (conflict, out-of-date, lock bị cướp, mixed-revision)
│   ├── Activity panel (thao tác đang chạy/đã xong, lỗi)
│   └── Status bar
├── Dialogs: Checkout · Add WC · Sign in · Trust certificate · Switch/Create branch · Create tag
│            Update to revision · Choose folders (sparse) · Lock · Discard confirm · Ignore
│            Properties · Repository layout · Cleanup · Upgrade format · Merge (P6)
├── Conflict resolver (cửa sổ riêng)
└── Settings (General · Accounts · Appearance · Diff & Merge · Integrations · Network · Advanced)
```

## 3. Cửa sổ chính

### 3.1 Bố cục
```
┌──────────────────────────────────────────────────────────────────────────────────────┐
│ [▣ game-assets ▾]  [⑂ trunk  r48211 ▾]  [⟳ Update  ↓3]                [⧗ 1]  [⚙]     │ Toolbar
├───────────────────────────────┬──────────────────────────────────────────────────────┤
│ Changes 12 │ History │ Browse │ src/Player.cs                    [Split|Unified] [⋯] │
│ ┌ Filter files…          [⚲]┐│ ─────────────────────────────────────────────────────│
│ ☑ M  src/Player.cs           ││  41  41   public void Move() {                       │
│ ☑ A  src/Inventory.cs        ││  42     -   speed = 5;                               │
│ ☐ ?  notes.txt               ││      42 +   speed = config.Speed;                    │
│ ☑ M🔒 art/hero.psd    (bạn)  ││  43  43   }                                          │
│ ☐ C  data/levels.json  ⚠     ││                                                      │
│ ▸ Changelist: refactor (3)   ││                                                      │
│ ─────────────────────────────││                                                      │
│ Summary (bắt buộc)           ││                                                      │
│ Description                  ││                                                      │
│ ☐ Giữ lock sau commit        ││                                                      │
│ [ Commit 3 files to trunk ]  ││                                                      │
├───────────────────────────────┴──────────────────────────────────────────────────────┤
│ D:\work\game-assets · r48211 · ✓ up to date 2 phút trước · hxd@corp · 1 tác vụ nền  │ Status bar
└──────────────────────────────────────────────────────────────────────────────────────┘
```
- Sidebar trái rộng mặc định 320px, kéo được (220–600px). Diff chiếm phần còn lại.
- Cửa sổ tối thiểu 960×600. Dưới 1100px: sidebar thu về 260px.

### 3.2 Toolbar
| Thành phần | Hành vi |
|---|---|
| **Current repository ▾** (Working copy) | Popover (`Mod+T`): ô lọc + danh sách WC (tên, đuôi đường dẫn, host repo, lần mở gần nhất; mũi tên/Enter để chuyển, `Shift+F10` hoặc chuột phải/⋯ cho thao tác §6.5: Reveal, Copy URL, Rename, Remove; WC mất thư mục có cảnh báo + "Locate…"). Chân popover: nút "Checkout…", "Add existing…". *Giai đoạn sau (chưa làm):* nhóm theo server và trạng thái tóm tắt mỗi WC (số thay đổi, ↓ incoming). Right-click: context menu WC (§6.5). |
| **Location ▾** (đổi nhãn thành **Branch ▾** khi repo có layout) | Hai chế độ (xem §3.9). **Có layout**: popover cây trunk/branches/tags, tìm kiếm, branch gần đây; nút "New branch…", "New tag…". **Không có layout**: hiện đường dẫn WC trong repo (vd `/2. Sản xuất/2. Phát triển`), nút "Switch to another folder…" (chọn URL bất kỳ trong repo), "Copy to new location…", "Set up branch layout…". Chọn → xác nhận Switch (cảnh báo nếu có thay đổi local). Luôn hiện revision; mixed-revision hiện "r48200–48211". |
| **Update** (nút đa trạng thái) | `Check for updates` (chưa biết) → `Update ↓N` (có N revision mới; hover: danh sách revision) → `Updating 45%` (bấm để xem Activity / hủy) → `Up to date`. Mũi tên phụ ▾: Update to revision…, Update selected folders, Check now. |
| **Activity ⧗N** | Mở/đóng Activity panel; badge số tác vụ đang chạy; đỏ nếu tác vụ lỗi. |
| **⚙** | Settings. |

### 3.3 Tab Changes
- **Danh sách file**: checkbox (chọn để commit), icon trạng thái (§7), đường dẫn tương đối (thư mục mờ, tên đậm), badge lock, badge conflict.
- **Nhóm**: theo changelist (mặc định nhóm "Không có changelist" trước), có thể chuyển "Dạng cây thư mục".
- **Filter**: theo text; chip lọc nhanh: Modified · Added · Unversioned · Conflicted · Locked.
- **Hành vi tự động**: file unversioned không tự tick (tránh commit rác); tick file `?` → engine `add` lúc commit; file `!` (missing) tick → `delete`. Phát hiện rename (missing + unversioned cùng nội dung) → gợi ý "Ghi nhận là move".
- **Checkbox đầu danh sách**: chọn/bỏ tất cả (trạng thái tristate).
- **Commit box**: Summary (bắt buộc, cảnh báo > 72 ký tự), Description (đa dòng), lịch sử message gần đây (↑ trong ô Summary), tùy chọn "Giữ lock sau commit". Nút: `Commit N files to <branch>`; disabled khi 0 file hoặc có conflict trong file đã chọn. `Mod+Enter` để commit.
- **Sau commit**: toast "Đã commit r48212" + link xem trong History. Nếu out-of-date → banner "Server có thay đổi mới hơn — Update rồi commit lại" + nút Update.
- **Diff pane**: Split/Unified; Hide whitespace; wrap; file binary → thông tin kích thước/mime + "Mở bằng ứng dụng mặc định"; ảnh → side-by-side / onion / swipe; thay đổi property hiển thị dưới diff nội dung. Đầu pane có nút "⋯" (menu file, §6.1).
- **Empty state**: "Không có thay đổi local" + gợi ý: Open in Explorer/Finder, Open in editor, phím tắt.

### 3.4 Tab History
- **Scope**: mặc định toàn WC; chọn file/thư mục (từ Changes/Browse → "Show log") để lọc; chip hiển thị scope + nút xóa.
- **Danh sách revision** (virtualized, tải dần 100/lần): `r48211 · hxd · 2 giờ trước`, dòng đầu message; icon: ↓ chưa update (incoming), 🏷 tạo tag, ⑂ tạo branch, ⇄ merge.
- **Filter**: message/author/path, khoảng ngày; "Stop on copy" (toggle).
- **Chi tiết revision** (pane phải): header (rev, author, ngày giờ đầy đủ, message), danh sách changed paths (A/M/D/R, copy-from), click path → diff của revision đó.
- **Chọn 2 revision** (Shift/Mod click) → so sánh khoảng.

### 3.5 Tab Browse
- **Thanh trên**: URL hiện tại (breadcrumb, copy được) + chọn revision (HEAD / số / ngày).
- **Cây bên trái**: thư mục repo, branch/tag hiển thị icon riêng; lazy-load từng cấp.
- **Pane phải** theo lựa chọn: thư mục → bảng (tên, kích thước, rev, author, ngày, lock); file → tab Preview · Blame · Log.
- Thao tác trực tiếp lên server (commit ngay, cần message): New folder, Rename, Delete, Copy (tạo branch/tag), Upload file — luôn qua dialog có ô message.

### 3.6 Banner (trên cùng nội dung tab)
| Banner | Màu | Hành động |
|---|---|---|
| N file đang conflict | Vàng | Resolve… (mở Conflict resolver) |
| Commit bị từ chối do out-of-date | Vàng | Update now |
| Lock của bạn trên X đã bị người khác lấy | Đỏ | Xem chi tiết |
| Working copy mixed-revision | Xanh | Update toàn bộ |
| WC bị khóa / cần cleanup | Đỏ | Run cleanup |
| WC format cũ (<31) | Xám | Hướng dẫn upgrade bằng svn CLI |

### 3.7 Activity panel
- Dock dưới (cao 200px, kéo được). Mỗi dòng: icon loại, mô tả ("Update game-assets"), tiến trình (file/bytes, tốc độ), thời gian, nút Cancel.
- Tác vụ xong giữ 50 mục gần nhất; lỗi mở rộng xem chi tiết (mã lỗi svn, URL) + nút hành động đề xuất + "Copy chi tiết".

### 3.8 Status bar
`đường dẫn WC · revision · trạng thái cập nhật (lần check cuối) · tài khoản · số tác vụ nền · cảnh báo format (32 = "pristine on demand")`. Click từng mục mở chức năng tương ứng.

### 3.9 Branch/tag trong SVN — hai chế độ hiển thị
SVN **không có** đối tượng branch/tag: đó chỉ là thư mục được tạo bằng `svn copy` theo quy ước (`trunk/`, `branches/`, `tags/`). Nhiều repo nội bộ tổ chức theo thư mục tài liệu/dự án và không có quy ước này. Vì vậy app không giả định branch tồn tại.

| | Có layout (phát hiện được hoặc người dùng cấu hình) | Không có layout |
|---|---|---|
| Toolbar | `Branch ▾` · tên branch/tag (`trunk`, `feature-x`, `🏷 v2.4.0`) | `Location ▾` · đường dẫn WC trong repo, rút gọn giữa nếu dài |
| Popover | Cây trunk/branches/tags | Đường dẫn hiện tại + thư mục cha/anh em gần đây; "Switch to another folder…" |
| Tạo mới | New branch / New tag (copy vào `branches/` / `tags/`) | "Copy to new location…" (copy URL→URL bất kỳ, kèm message) |
| Menu Branch | Đầy đủ | Đổi tên thành **Location**: Switch to another folder… · Copy to new location… · Set up branch layout… · Compare with another folder… |
| History | Icon ⑂ / 🏷 cho revision tạo branch/tag | Revision có copy hiện icon ⧉ "copy từ …@r" |

**Phát hiện layout tự động** khi thêm WC: tìm các segment `trunk`, `branches`, `tags` (không phân biệt hoa/thường) trên đường dẫn từ gốc repo tới WC, hỗ trợ dạng nhiều project (`/ProjectA/trunk`). Không thấy → chế độ "Không có layout"; người dùng có thể cấu hình tay (§4.11). Lưu theo `repo_uuid` trong `app.sqlite`.

**Tag chỉ đọc theo quy ước:** WC đang ở `tags/…` → banner xám "Bạn đang ở tag v2.4.0 — tag thường không được sửa"; nút Commit đổi thành "Commit vào tag…" và yêu cầu xác nhận. Server có thể chặn bằng phân quyền/hook — lỗi 403 hiển thị rõ.

## 4. Dialogs

### 4.1 Checkout
| Trường | Ghi chú |
|---|---|
| Repository URL | autocomplete URL gần đây; nút "Browse…" mở trình duyệt repo mini |
| Thư mục đích | mặc định `<thư mục gốc cấu hình>/<tên repo>`; cảnh báo nếu không rỗng |
| Revision | HEAD / số / ngày |
| Phạm vi | Toàn bộ · Chọn thư mục… (mở dialog sparse §4.8) |
| Định dạng WC | MVP: luôn `Tương thích svn 1.8+ (format 31)` (ẩn lựa chọn). Sau MVP: thêm `Tiết kiệm dung lượng — cần svn 1.15+ (format 32)` |
| Externals | ☐ Bỏ qua externals |
Nút: Checkout (chạy nền, mở Activity) · Cancel. Hiển thị ước tính dung lượng khi đã chọn phạm vi.

### 4.2 Add existing working copy
Chọn thư mục → engine đọc `.svn/wc.db` → hiện URL, revision, format; lỗi rõ ràng nếu format < 31 hoặc không phải WC.

### 4.3 Sign in
- Hiện realm + URL server.
- Windows (máy join domain): nút chính "Đăng nhập bằng tài khoản Windows" (SSO); liên kết phụ "Dùng tài khoản khác".
- Username / Password, ☑ Ghi nhớ (lưu vào OS credential store). Lỗi 401 lặp lại → thông báo rõ, không khóa tài khoản (tối đa 3 lần rồi dừng).

### 4.4 Trust certificate
Hiển thị host:port, subject, issuer, hiệu lực, SHA-256 fingerprint, lỗi cụ thể. Bộ nút theo loại lỗi (đóng dialog hoặc Esc luôn = Từ chối):
| Lỗi | Nút | Ghi chú |
|---|---|---|
| CA không tin cậy (`unknown-issuer`) / tự ký (`self-signed`) | **Từ chối** · **Tin tạm thời** (phiên này) · **Tin vĩnh viễn** (pin host:port + fingerprint leaf) | Chỉ hai lỗi này được phép tin |
| Sai hostname / hết hạn / chưa có hiệu lực / bị thu hồi / lỗi khác | **Đóng** | Chặn cứng, kèm hướng dẫn liên hệ IT. Cert vừa đổi vừa sai hostname được báo là sai hostname |
| Cert của host đã pin bị đổi (`changed`) | **Từ chối** · **Thay chứng chỉ đã ghim** | Cảnh báo riêng "Chứng chỉ máy chủ đã thay đổi", hiện fingerprint cũ và mới. Không có "Tin tạm thời" và không có "Tin vĩnh viễn" một cú click: nút thay pin chỉ bật sau khi tick "Tôi đã đối chiếu vân tay mới với quản trị viên máy chủ" |

### 4.5 Switch / New branch / New tag / Copy to new location
- Switch: chọn từ cây (có layout) hoặc nhập/duyệt URL bất kỳ trong cùng repo (không có layout; `svn switch` yêu cầu cùng repository UUID); nếu có thay đổi local → "Mang thay đổi sang branch mới" (svn switch giữ thay đổi) hoặc "Hủy".
- New branch: tên (validate ký tự), nguồn (`HEAD của branch hiện tại` · `Revision cụ thể` · `Working copy hiện tại (gồm thay đổi local)`), message, ☑ Switch sang branch mới.
- New tag: tương tự, không có tùy chọn switch.
- Copy to new location (không có layout): nguồn (URL hiện tại hoặc chọn), đích (duyệt cây repo + tên mới), revision, message, ☐ Switch sang bản sao. Thực chất là `svn copy URL URL` — rẻ, không nhân dung lượng trên server.

### 4.6 Discard changes (xác nhận)
"Bạn sắp hủy thay đổi của N file. Không thể hoàn tác." + danh sách file; ☐ Xóa cả file unversioned. Nút nguy hiểm màu đỏ.

### 4.7 Lock
Comment (tùy chọn), danh sách file; nếu đã bị người khác lock: hiện chủ lock + thời gian, nút "Steal lock" (xác nhận lần 2). Unlock: ☐ Break lock của người khác (cần quyền).

### 4.8 Choose folders (sparse checkout)
Cây thư mục server với checkbox 3 trạng thái + cột dung lượng ước tính; chế độ cho từng nút: Toàn bộ · Chỉ file · Không tải. Áp dụng → `set-depth` chạy nền.

### 4.9 Ignore
Lựa chọn: file này · mọi file `*.ext` trong thư mục · thư mục; phạm vi: `svn:ignore` của thư mục (chia sẻ với team, cần commit) hoặc ignore cục bộ (`global-ignores` của máy).

### 4.10 Properties
Bảng key/value cho file/thư mục; edit dạng text cho giá trị nhiều dòng (externals, ignore); xem diff trước khi lưu; thuộc tính phổ biến có gợi ý (`svn:needs-lock`, `svn:eol-style`, `svn:mime-type`, `svn:externals`, `svn:ignore`, `svn:keywords`).

### 4.11 Repository layout
Ba lựa chọn: **Tự phát hiện** (mặc định) · **Tùy chỉnh** (đường dẫn trunk / branches / tags, hỗ trợ nhiều project: `/{project}/trunk`) · **Không dùng branch** (luôn ở chế độ Location). Dùng cho toolbar, menu Branch/Location, Browse và History.

### 4.12 Khác
Update to revision · Cleanup (tùy chọn: chạy work queue, xóa file unversioned/ignored, vacuum pristine) · Upgrade format (31 → 32, cảnh báo tool cũ không đọc được) · Merge (phase 6: nguồn, revision eligible, preview).

## 5. Conflict resolver (cửa sổ riêng)
```
┌ Conflicts (3) ──────────┬───────────────────────────────────────────────────────────┐
│ ⚠ data/levels.json      │  Của bạn (mine)         │  Kết quả            │ Server (theirs) │
│ ⚠ src/Player.cs         │  ...                     │  ...                │ ...             │
│ ⚠ art/ (tree conflict)  │  [← Lấy của bạn]         │  (sửa tự do)        │ [Lấy server →]  │
├─────────────────────────┴───────────────────────────────────────────────────────────┤
│ ◀ conflict trước  ▶ conflict sau  ·  Lấy toàn bộ của bạn · Lấy toàn bộ server  ·  [Đánh dấu đã giải quyết] │
└─────────────────────────────────────────────────────────────────────────────────────┘
```
- MVP: chọn Mine/Theirs cho cả file, mở editor ngoài, đánh dấu resolved.
- Phase 6: 3-way theo từng block, tree conflict có giải thích ("Bạn sửa file mà server đã xóa") + lựa chọn tương ứng.
- File binary: chỉ Mine/Theirs + xem 2 phiên bản.

## 6. Context menu

### 6.1 File trong Changes
| Mục | Phím | Điều kiện |
|---|---|---|
| Discard changes… | `Mod+Shift+Backspace` | có thay đổi |
| Ignore ▸ (file · `*.ext` · thư mục) | | unversioned |
| Add to changelist ▸ (danh sách · New changelist…) / Remove from changelist | | versioned |
| Lock… / Unlock | `Mod+L` | |
| Resolve ▸ (Mine · Theirs · Mở conflict resolver · Đánh dấu đã giải quyết) | | conflicted |
| Mark as move from… | | missing/unversioned |
| Show log | `Mod+Shift+L` | |
| Blame | `Mod+Shift+B` | versioned |
| Open with default app · Open in external editor | `Mod+Shift+A` (editor) | |
| Show in Explorer/Finder/File manager | `Mod+Shift+R` | |
| Copy path · Copy relative path · Copy URL | | |
| Rename/Move… · Delete | | versioned |
| Properties… | | versioned |

### 6.2 Revision trong History
Copy revision number · Copy message · Compare with working copy · Update to this revision… · Revert changes from this revision… · Create branch/tag from this revision… · Merge this revision into current branch… (P6) · Open in VisualSVN web (nếu cấu hình URL web).

### 6.3 Changed path trong chi tiết revision
Show diff · Open file at this revision · Save as… · Blame at this revision · Show log of this path.

### 6.4 Node trong Browse
Checkout… · Show log · Blame (file) · Download/Save as… · Copy URL · Lock/Unlock (file) · New folder… · Upload file… · Rename… · Delete… · Create branch/tag from here… · Properties…

### 6.5 Working copy (popover Working copy ▾)
Show in Explorer/Finder · Open in terminal · Open in editor · Cleanup… · Repository layout… · Remove from list (không xóa file) · Đổi tên hiển thị.

## 7. Biểu tượng trạng thái
| Ký hiệu | Màu | Ý nghĩa | svn |
|---|---|---|---|
| M | cam | Đã sửa | `M` |
| A | xanh lá | Thêm mới | `A` |
| D | đỏ | Xóa | `D` |
| R | tím | Thay thế / move | `R` / moved |
| ? | xám | Chưa quản lý | `?` |
| ! | đỏ | Mất file | `!` |
| C | vàng đậm | Conflict | `C` |
| 🔒 | xanh dương (bạn) / đỏ (người khác) | Lock | `K` / `O` |
| ↓ | xanh | Có bản mới trên server | `*` (status -u) |
| P | xám | Chỉ đổi property | cột 2 `M` |
Mọi icon kèm tooltip chữ (không chỉ dựa vào màu).

## 8. Menu bar

### 8.1 macOS — menu ứng dụng `OpenSVN`
About OpenSVN · Check for app updates… · Settings… `Cmd+,` · Hide `Cmd+H` · Quit `Cmd+Q`.
(Windows/Linux: Settings nằm ở File; About/Check for app updates ở Help.)

### 8.2 File
| Mục | Phím |
|---|---|
| Checkout repository… | `Mod+Shift+O` |
| Add existing working copy… | `Mod+O` |
| Switch repository… (mở popover **Current repository ▾**, như GitHub Desktop) | `Mod+T` |
| Open recent ▸ | |
| Close working copy | `Mod+W` |
| Settings… (Win/Linux) | `Ctrl+,` |
| Exit (Win/Linux) | `Alt+F4` / `Ctrl+Q` |

### 8.3 Edit
Undo `Mod+Z` · Redo `Mod+Shift+Z` · Cut · Copy · Paste · Select all `Mod+A` · Find / Filter `Mod+F` (trong danh sách = lọc file/revision, trong diff = tìm text) · Copy path `Mod+Alt+C` · Copy URL.

### 8.4 View
| Mục | Phím |
|---|---|
| Changes · History · Browse | `Mod+1` · `Mod+2` · `Mod+3` |
| Activity panel | `Mod+J` |
| Toggle sidebar | `Mod+\` |
| Diff: Split / Unified | `Mod+Alt+D` (đổi) |
| Hide whitespace changes | `Mod+Alt+W` |
| Show unversioned files · Show ignored files | |
| Group by changelist · Show as tree | |
| (4 mục trên bật/tắt danh sách Changes, nhớ theo người dùng; cũng có trong nút "Tùy chọn hiển thị" của danh sách. File ignored hiện glyph `I`, không tick để commit được) | |
| Theme ▸ System · Light · Dark | |
| Zoom in · Zoom out · Actual size | `Mod+=` · `Mod+-` · `Mod+0` |
| Toggle full screen | `F11` / `Ctrl+Cmd+F` |

### 8.5 Working Copy
| Mục | Phím |
|---|---|
| Check for updates | `Mod+Shift+K` |
| Update | `Mod+Shift+U` |
| Update to revision… | |
| Commit | `Mod+Enter` |
| Discard all changes… | |
| Choose folders (sparse)… | |
| Cleanup… | |
| Properties… | |
| Repository layout… | |
| Upgrade working copy format… | |
| Show in Explorer/Finder | `Mod+Shift+R` |
| Open in terminal | `` Mod+` `` |
| Open in editor | `Mod+Shift+A` |

### 8.6 Branch (repo có layout) / Location (không có layout)
- **Branch:** Switch branch… `Mod+B` · New branch… `Mod+Shift+N` · New tag… `Mod+Shift+T` · Merge into current branch… `Mod+Shift+M` (P6) · Compare with branch… · Show branch on server (Browse) · Set up branch layout…
- **Location:** Switch to another folder… `Mod+B` · Copy to new location… `Mod+Shift+N` · Compare with another folder… · Show on server (Browse) · Set up branch layout…
- **Compare with branch / another folder:** so sánh URL của WC với một branch/tag hoặc URL thư mục bất kỳ trong cùng repo, cả hai ở HEAD (không tính thay đổi local). Mỗi lần một thư mục: mục chỉ có ở một bên, mục đã đổi (khác last-changed revision hoặc kích thước; bản copy giữ nguyên revision của nguồn cho node chưa sửa), mục không đổi ẩn mặc định. Mở thư mục đã đổi để đi sâu; bấm file đã đổi để xem diff hai bên.

### 8.7 Selection (áp dụng file/node đang chọn — giống context menu §6.1)
Discard changes… · Lock… `Mod+L` · Unlock · Add to changelist ▸ · Ignore ▸ · Resolve ▸ · Show log `Mod+Shift+L` · Blame `Mod+Shift+B` · Rename/Move… · Delete · Properties…

### 8.8 Window (macOS) / Help
- Window: Minimize `Cmd+M` · Zoom · Conflict resolver · Bring all to front.
- Help: OpenSVN documentation · Keyboard shortcuts `Mod+/` · Show logs folder · Report a problem… (tạo gói log, không gửi tự động) · About (Win/Linux) · Check for app updates (Win/Linux).

## 9. Settings
| Trang | Nội dung |
|---|---|
| General | Thư mục gốc checkout mặc định; tự check updates mỗi N phút (mặc định 10, 1–1440; hiện chưa có chế độ tắt); mở WC gần nhất khi khởi động; ngôn ngữ (vi/en) |
| Accounts | Danh sách realm/server đã đăng nhập (user, phương thức Basic/Windows SSO), Sign out; danh sách chứng chỉ đã tin (xóa được) |
| Appearance | Theme, font UI/diff, cỡ chữ, mật độ danh sách (compact/comfortable) |
| Diff & Merge | Split/Unified mặc định, hide whitespace, tab width, công cụ diff/merge ngoài (Beyond Compare, Meld, KDiff3, VS Code…) |
| Integrations | Editor ngoài, terminal; **mẫu URL web VisualSVN**: file/thư mục `https://{server}/!/#{repo}/view/{revSpec}/{path}` (đã đối chiếu server thật; `{revSpec}` = `head` \| `r<N>`, `{path}` tính từ gốc repo, không có `/` đầu, mã hóa UTF-8 từng đoạn); revision `https://{server}/!/#{repo}/commit/r{rev}/` (chưa đối chiếu) |
| Network | Proxy (system/manual), timeout, số kết nối song song (mặc định 4, tối đa 8: giới hạn của transport HTTP) |
| Advanced | Định dạng WC mặc định khi checkout; global-ignores; đọc config `~/.subversion` (on/off); mức log; xóa cache |

## 10. Luồng tương tác chính

### 10.1 Lần đầu dùng
Welcome → Checkout → nhập URL → (Trust cert nếu cần) → Sign in (SSO hoặc user/pass) → chọn thư mục + phạm vi → Activity hiện tiến trình → xong mở tab Changes (trống) + toast.

### 10.2 Commit hằng ngày
Sửa file → watcher cập nhật Changes (≤1s) → tick file → xem diff → nhập Summary → `Mod+Enter` → Activity "Committing" → toast r#### → danh sách trống.
Nhánh lỗi: out-of-date → banner + Update → (conflict → resolver) → commit lại.

### 10.3 Update có conflict
Nút `Update ↓3` → tiến trình → xong: banner "2 file conflict" → Resolve… → resolver → Mark resolved → banner biến mất.

### 10.4 Sửa file binary có needs-lock
File hiển thị 🔒 xám (read-only) → double-click/“Lock để sửa” → dialog Lock → thành công: file writable, badge 🔒 xanh → sửa → commit (mặc định nhả lock) . Nếu người khác giữ lock: dialog hiện chủ lock, gợi ý liên hệ, nút Steal (xác nhận 2 lần).

### 10.5 Chuyển branch / thư mục
Không có layout: Location ▾ → Switch to another folder… → duyệt cây repo → chọn thư mục → như dưới.
Có layout: Branch ▾ → chọn `branches/feature-x` → nếu có thay đổi local: dialog "Mang thay đổi theo" / "Hủy" → Activity "Switching" → toolbar cập nhật.

## 11. Thông báo
| Sự kiện | Hình thức |
|---|---|
| Commit thành công | Toast trong app (5s) |
| Có revision mới (check định kỳ) | Badge nút Update; thông báo OS nếu app ở nền (tắt được) |
| Lock bị người khác steal | Banner đỏ + thông báo OS |
| Tác vụ nền lỗi | Badge đỏ Activity + toast có nút "Xem" |
| Tác vụ dài (>30s) xong khi app ở nền | Thông báo OS |
