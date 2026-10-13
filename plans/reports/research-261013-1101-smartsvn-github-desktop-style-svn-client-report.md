# Nghiên cứu SmartSVN → Thiết kế SVN client kiểu GitHub Desktop

Ngày: 2026-10-13

## 1. SmartSVN là gì

- SVN client đa nền tảng (Win/macOS/Linux) của syntevo (cùng hãng SmartGit). App desktop Java.
- Hỗ trợ working copy + protocol SVN 1.14, gần như toàn bộ lệnh svn.
- Bản mới nhất: 14.5 (native Apple Silicon, ẩn thay đổi whitespace, dialog externals).
- License: free cho open-source/sinh viên/học thuật; thương mại phải trả phí. → Khoảng trống: chưa có client SVN hiện đại, đơn giản, miễn phí.

## 2. Bản đồ tính năng SmartSVN

| Nhóm | Tính năng | Ghi chú cho app mới |
|---|---|---|
| Working copy | Project view: cây thư mục, mũi tên đỏ = local changes, xanh = remote changes; Remote State từng file | Cốt lõi — `svn status -u --xml` |
| | Explorer/Finder integration, icon overlay | Bỏ ở MVP (phức tạp, native) |
| | Sửa properties (svn:ignore, externals...) dạng text + diff | Phase 3 |
| | Output view: log lệnh, highlight conflict | Nên có (panel "Activity") |
| | Vacuum/Cleanup, Fix line-endings | Cleanup: MVP; còn lại sau |
| Commit | Gợi ý add file mới / remove file mất, phát hiện move/rename | MVP — giống GH Desktop tự stage |
| | Diff tích hợp khi viết message, bỏ chọn file | MVP — đây là trải nghiệm GH Desktop |
| | Commit cả externals (chung/riêng message) | Phase 3 |
| | Spell-check, Bugtraq link, chọn JIRA issue | Phase 4 |
| | **Change Sets** (nhóm file commit riêng, kéo-thả) | Map sang `svn changelist` — Phase 2 |
| Log | Lịch sử file/thư mục: message, author, changed paths; mở diff; rollback revision | MVP (tab History) |
| | **Revision Graph**: cây lịch sử, trạng thái merged/unmerged, theo dõi copy/move | Phase 3 — tính năng khác biệt lớn |
| Branch/Tag | Khai báo trunk/branches/tags, Tag Browser dạng cây, switch không gõ URL | MVP — dropdown "Current branch" như GH Desktop |
| Merge | Merge giữa branch, mergeinfo | Phase 2 (merge range/reintegrate) |
| Conflict | Conflict Solver 3-way, sửa tự do kết quả; cảnh báo conflict sớm | MVP: resolve mine/theirs + mở editor; 3-way UI Phase 2 |
| Repo Browser | Duyệt repo theo revision, checkout, mkdir/move/rename/copy/delete, drag-drop | Phase 2 |
| | Annotate/Blame tô màu theo tuổi dòng | Phase 2 |
| Transactions | Tự fetch revision mới (cả branch khác/thư viện dùng chung), đánh dấu unread | Phase 2 — kiểu "notification" |
| Compare | File compare inline có sửa, syntax highlight, image compare, unified/side-by-side | MVP dùng Monaco diff |
| | Change Report đa file | MVP (gộp trong Changes view) |
| Khác | Locking (needs-lock), dark theme, tìm kiếm preferences | Lock: Phase 2 (quan trọng với file binary/game) |

## 3. Triết lý GitHub Desktop áp dụng cho SVN

GH Desktop thắng nhờ **ít khái niệm, 1 màn hình chính**. Ánh xạ:

| GH Desktop | SVN tương ứng |
|---|---|
| Repo list (sidebar) | Danh sách working copy |
| Current branch dropdown | Branch/Tag browser + `svn switch` |
| Fetch / Pull | `svn status -u` (check remote) / `svn update` |
| Changes tab + checkbox | `svn status`, chọn file → commit (không có staging; checkbox = danh sách path truyền vào `svn commit`) |
| Commit + Push | **Gộp 1 bước** `svn commit` (SVN tập trung) — đơn giản hơn Git |
| History tab | `svn log --xml -v` + diff revision |
| Stash | `svn changelist` hoặc `svn x-shelve` (1.14, experimental) — cân nhắc tự implement bằng patch |
| Discard changes | `svn revert` |
| Clone | `svn checkout` (hỗ trợ sparse `--depth`) |

Khác biệt SVN cần UI riêng: **conflict sau update** (không phải lúc merge), **lock file**, **externals**, **mixed-revision working copy**, commit thất bại do out-of-date.

## 4. Kiến trúc đề xuất

### Lựa chọn engine SVN
| Phương án | Ưu | Nhược |
|---|---|---|
| **A. Gọi `svn` CLI + `--xml`** (khuyến nghị) | Ổn định, đúng chuẩn 1.14, status/log/info/list/blame/diff --summarize đều có XML; dễ debug | Cần bundle svn binary (Win: SlikSVN/TortoiseSVN CLI; mac: brew); parse XML; diff là text |
| B. Bind libsvn C (native addon/Rust FFI) | Nhanh, kiểm soát tốt | Build đa nền tảng khó (APR, OpenSSL, Serf); node-svn đã archived |
| C. SVNKit (Java) | Pure Java, như SmartSVN cũ | Kéo JVM; license TMate (copyleft hoặc mua) |

### Stack
- **Tauri 2 (Rust backend) + React/TS + Monaco diff**: app nhẹ (~10–20MB), Rust spawn `svn`, parse XML (quick-xml), stream progress qua event.
- Thay thế: Electron + Node (`child_process`, xml parser) — nhanh dev hơn nếu team mạnh JS, app nặng hơn.
- Lưu trữ local: SQLite (danh sách repo, cache log, cấu hình trunk/branches/tags, notification).
- Credentials: OS keychain (Rust `keyring`), truyền qua `--password-from-stdin`, luôn `--non-interactive`; xử lý cert lỗi qua dialog → `--trust-server-cert-failures`.

### Module
```
core/svn-runner        spawn, timeout, cancel, env LANG=C, stdin password
core/xml-parsers       status, log, info, list, blame, mergeinfo
core/working-copy      model trạng thái, watcher FS (notify) → refresh status
features/changes       danh sách file, checkbox, diff, commit
features/history       log phân trang, diff revision, revert-to-revision
features/branches      layout trunk/branches/tags, switch, create branch (svn copy)
features/conflicts     detect, resolve (mine/theirs/working), 3-way editor
features/repo-browser  svn list, ops từ xa
ui/                    sidebar repo, toolbar (Update / Commit / Branch), activity panel
```

## 5. Lộ trình

- **MVP (6–8 tuần, 1–2 dev)**: thêm/checkout repo, Changes + diff + commit (auto add/delete), Update, Revert, History, switch branch, conflict cơ bản, Cleanup, auth + keychain, dark mode.
- **Phase 2**: Changelist (Change Sets), merge, 3-way conflict solver, repo browser, blame, lock/unlock, theo dõi revision mới (Transactions) + thông báo.
- **Phase 3**: Revision Graph, externals, properties editor, sparse checkout UI, shelve.
- **Phase 4**: tích hợp JIRA/bugtraq, spell-check, Explorer/Finder overlay, auto-update.

## 6. Rủi ro

- Bundle svn CLI theo OS + kiểm tra phiên bản working copy format (1.8+ vs 1.14).
- Repo lớn: `svn status -u` / `svn log` chậm → cache SQLite, phân trang `--limit`, chạy nền.
- Locale output: ép `LANG=C`/`LC_MESSAGES=C` để parse lỗi.
- Revision Graph tốn chi phí (cần quét log toàn repo + copyfrom) — để sau.
- Không copy UI/branding SmartSVN; chỉ tham khảo tính năng.

## Câu hỏi chưa giải quyết
1. Nền tảng mục tiêu: Windows là chính hay cả macOS/Linux?
2. Stack ưu tiên của team: Tauri/Rust hay Electron/Node (hoặc .NET/WPF nếu chỉ Windows)?
3. Server SVN dùng: VisualSVN/Apache (http/https), svnserve, svn+ssh? Có cần auth domain/AD?
4. Quy mô repo & loại file (nhiều binary → lock quan trọng)?
5. Dùng nội bộ hay phát hành thương mại/open-source (ảnh hưởng license lib, bundle svn)?
6. Có cần tích hợp JIRA nội bộ ngay từ đầu?

## Nguồn
- https://smartsvn.com/features
- https://www.smartsvn.com/whats-new
- https://blog.syntevo.com/smartsvn/2023/04/12/smartsvn-14-3-released.html
- https://svnkit.com/licensing
- https://www.npmjs.com/package/svn-interface
- https://github.com/yume-chan/node-svn
