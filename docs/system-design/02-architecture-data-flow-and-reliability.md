# 02 — Kiến trúc, luồng dữ liệu & độ tin cậy

Cập nhật: 2026-10-13 · Liên quan: [01](01-ui-screens-menus-and-interactions.md), [03](03-ipc-api-contract-and-data-model.md), [ADR](../system-architecture.md)

## 1. Yêu cầu

### 1.1 Chức năng (MVP → P6)
| Nhóm | Yêu cầu | Phase |
|---|---|---|
| Kết nối | HTTPS tới VisualSVN; Basic + Negotiate (SSO Windows); trust cert; proxy | 1 |
| Remote | info, list, cat, log, blame, lock/unlock, tạo branch/tag, thao tác trực tiếp trên server | 2 |
| Working copy | checkout (sparse), update/switch, status local+remote, commit, revert, add/delete/move, changelist, cleanup, ignore, props | 3 |
| App | 3 tab Changes/History/Browse, dialogs, Activity, nhiều WC | 4 |
| Repo lớn | tải song song, format 32 pristine-on-demand, cache log, lock UX, theo dõi revision mới | 5 |
| Nâng cao | merge/mergeinfo, conflict 3-way, tree conflict, externals, revision graph, shelve | 6 |

### 1.2 Phi chức năng
| Thuộc tính | Mục tiêu |
|---|---|
| Tương thích | WC đọc/ghi được hai chiều với svn 1.8–1.15, TortoiseSVN, SmartSVN |
| Toàn vẹn dữ liệu | 0 mất thay đổi local; crash ở bất kỳ điểm nào → phục hồi bằng work queue |
| Khởi động | Cửa sổ hiện < 1s; WC gần nhất có status (từ cache) < 2s |
| Status local | WC 100k file: lần đầu < 15s (SSD), lần sau < 3s, phản ứng thay đổi file < 1s |
| Checkout/update | MVP: không chậm hơn svn CLI 1.15 cùng mạng; sau MVP: ≥ 30% nhanh hơn (tải song song, GET delta) |
| UI | 60 fps khi cuộn list 100k dòng; thao tác UI không chờ I/O |
| Bộ nhớ | < 400MB RAM với WC 100k file; stream file lớn không nạp vào RAM |
| Bảo mật | Không lưu mật khẩu plaintext; TLS luôn verify; log không chứa credential |
| Khả năng quan sát | Log có cấu trúc, xoay vòng, xuất được "gói chẩn đoán" |

### 1.3 Ràng buộc & giả định
- 1 server VisualSVN (có thể nhiều repo); mạng LAN/VPN, độ trễ 1–50ms.
- Repo: tới ~1M revision, cây ~500k node, file tới vài GB.
- Không có backend riêng của OpenSVN — app là client thuần.
- Người dùng có thể dùng song song TortoiseSVN trên cùng WC.

## 2. Thiết kế tổng thể

### 2.1 Thành phần
```
┌─────────────────────────────── Process OpenSVN ───────────────────────────────┐
│  WebView (React)                                                              │
│   views ─ stores (Zustand) ─ queries (TanStack Query) ─ ipc client (specta)   │
│        ▲ events                          │ commands                           │
│ ───────┼─────────────────────────────────▼──────────────────────────────────── │
│  App layer (src-tauri)                                                        │
│   ├ CommandRouter      map command → service                                  │
│   ├ OperationManager   id, tiến trình, cancel, lịch sử 50 tác vụ              │
│   ├ WcRegistry         danh sách WC mở, mỗi WC 1 WcActor (hàng đợi tuần tự)   │
│   ├ StatusService      cache status + watcher (notify) + debounce             │
│   ├ IncomingPoller     check revision mới định kỳ                             │
│   ├ AuthBroker         credential store, prompt UI, SSO                       │
│   └ AppDb              app.sqlite (WC list, layout, log cache, settings)      │
│ ───────────────────────────────────────────────────────────────────────────── │
│  Engine crates                                                                │
│   svn-client ─┬ svn-wc (wc.db, pristine, work queue, translate)               │
│               ├ svn-ra (session pool, auth, transport) ─ svn-protocol (sans-IO)│
│               ├ svn-delta (svndiff, editor)                                    │
│               └ svn-diff (diff, diff3, mergeinfo)                              │
└───────────────────────────────────────────────────────────────────────────────┘
        │ file system (.svn/wc.db, pristine, file làm việc)       │ HTTPS
        ▼                                                          ▼
   Ổ đĩa người dùng                                       VisualSVN Server
```

### 2.2 Mô hình đồng thời
- 1 tokio runtime multi-thread. Tác vụ CPU nặng (hash, diff, nén) chạy `spawn_blocking`/rayon pool riêng.
- **WcActor**: mỗi WC 1 actor nhận lệnh qua channel; **lệnh ghi** (update, commit, revert, switch, cleanup, lock…) chạy tuần tự; **lệnh đọc** (status, diff, log) đi qua actor với busy-retry; **không đổi journal mode của wc.db** (giữ như svn — xem ghi chú `svn_sqlite.c` trong plan MVP); khi lệnh ghi dài đang chạy, status trả cache + cờ "đang cập nhật". WAL chỉ dùng cho `app.sqlite`/`cache.sqlite`.
- Trước lệnh ghi: lấy `WC_LOCK` trong wc.db như svn (tương thích TortoiseSVN); nếu tool khác đang giữ → báo "Working copy đang được dùng bởi chương trình khác" + Retry.
- **RaSessionPool** theo (repo root URL, user): tối đa N kết nối HTTP (mặc định 4, cấu hình 1–8) dùng chung cho mọi WC cùng server.
- Cancel: `CancellationToken` truyền xuống mọi tầng; điểm hủy an toàn chỉ giữa các work-queue item (không bao giờ để file ghi dở).

### 2.3 Luồng dữ liệu chính

**Status (local)**
```
watcher event ─► debounce 300ms ─► gom thư mục bẩn ─► WcActor.read
   stat(file) so với NODES.translated_size/last_mod_time ─► khác → hash (SHA-1) so pristine
   ─► FileStatus[] ─► diff với cache ─► event `wc://status-changed` (chỉ delta)
```
Lần đầu mở WC: hiển thị cache status từ phiên trước (app.sqlite) ngay, quét nền rồi cập nhật.

**Update**
```
UI Update ─► ensure_authenticated (prompt nếu cần, CHƯA giữ lock) ─► WcActor.write ─► lấy WC_LOCK (+ ghi sổ sở hữu app.sqlite)
 ─► REPORT update-report (gửi base rev từng path, depth) 
 ─► nhận editor drive (stream XML) ─► với mỗi file: fetch nội dung (song song, GET/delta)
     ─► pristine mới vào .svn/tmp ─► nếu có sửa local: diff3 → kết quả/markers vào .svn/tmp (CHƯA đổi metadata)
     ─► MỘT transaction: đổi NODES base + ghi conflict skel (nếu có) + thêm WORK_QUEUE item cài file/marker
 ─► chạy work queue (rename atomic, set flags, translate eol/keywords)
 ─► nhả WC_LOCK ─► event status + toast
Hủy giữa pha mạng → abort_edit, thư mục còn `incomplete` (update lại tiếp tục); crash ở bất kỳ điểm nào không bao giờ để base mới + file làm việc dựa base cũ.
```

**Commit**
```
UI Commit(paths, message) ─► ensure_authenticated ─► WcActor.write ─► thu thập: committables (M/A/D, props, lock token)
 ─► kiểm tra: không conflict, không out-of-date cục bộ rõ ràng
 ─► ghi commit journal (state_dir của app) ─► POST !svn/me (create-txn) → journal ghi txn ─► với mỗi path: PUT svndiff fulltext từ file đọc lại được
                                         / MKCOL / DELETE / COPY / PROPPATCH (song song có giới hạn)
 ─► MERGE (gửi lock tokens, release-locks theo tùy chọn) ─► nhận new revision → journal ghi rev
 ─► transaction wc.db: cập nhật NODES base_rev, checksum, PRISTINE, xóa lock nếu nhả ─► xóa journal
 ─► lỗi giữa chừng ─► DELETE txn trên server, WC không đổi
```
Tính atomic: server chỉ có revision mới sau MERGE thành công; WC chỉ cập nhật sau khi nhận revision. Crash sau MERGE trước khi cập nhật WC → lần mở sau đọc commit journal có rev → **finalize** WC theo journal (không để file "A" giả hay tree conflict add-vs-add). Journal chỉ có txn → `abort_txn` dọn txn rò.

**Incoming poller**: mỗi N phút (mặc định 10) với WC đang mở: `PROPFIND`/`OPTIONS` lấy youngest rev → nếu > base → `log` khoảng mới (scope WC) → badge `Update ↓N`.

### 2.4 Lưu trữ
| Dữ liệu | Nơi lưu | Ghi chú |
|---|---|---|
| Metadata WC | `<wc>/.svn/wc.db` + `pristine/` | Chuẩn libsvn_wc, không thêm bảng (ADR-004) |
| Danh sách WC, layout, settings, URL gần đây, cert tin cậy | `app.sqlite` trong thư mục dữ liệu app | `%APPDATA%\OpenSVN`, `~/Library/Application Support/OpenSVN`, `$XDG_DATA_HOME/opensvn` |
| Cache log/list/blame | `cache.sqlite` (tách riêng, xóa được) | Key `(repo_uuid, path, rev)`; revision bất biến → cache vĩnh viễn |
| Cache status | `cache.sqlite` | Chỉ để hiển thị nhanh lúc mở; luôn xác minh lại |
| Mật khẩu | OS credential store (keyring-core) | Service `opensvn`; account = `scheme\|host\|port\|realm\|user` (mỗi trường escape `%`→`%25`, `\|`→`%7C` nên key là đơn ánh); proxy dùng realm `\u{1}proxy` |
| Log | `logs/opensvn.YYYY-MM-DD.log` | Xoay 7 ngày, tối đa 100MB |

## 3. Chi tiết

### 3.1 Chiến lược cache
- **Bất biến theo revision** (log entry, nội dung file tại rev, blame tại rev): cache không hết hạn; giới hạn dung lượng LRU (mặc định 2GB, cấu hình).
- **Theo HEAD** (list thư mục HEAD, youngest rev): TTL 60s hoặc invalidate khi app commit/update.
- **Status**: nguồn sự thật là wc.db + FS; cache chỉ là bản hiển thị tức thời.
- Frontend: TanStack Query key theo `(wcId, kind, params)`; invalidate bằng event từ backend (`wc://status-changed`, `wc://revision-changed`).

### 3.2 Xử lý lỗi
Phân loại lỗi engine → mã ổn định gửi UI (chi tiết ở [03](03-ipc-api-contract-and-data-model.md#4-lỗi)):
| Loại | Ví dụ | Retry | Hành động UI |
|---|---|---|---|
| Network tạm thời | timeout, reset, 502/503 | Tự động 3 lần, backoff 0.5/1/2s (chỉ request idempotent: GET, PROPFIND, REPORT, OPTIONS) | Nếu vẫn lỗi: "Mất kết nối" + Retry |
| Auth | 401, 403 | Không | Mở Sign in (401) / "Không có quyền" (403) |
| TLS | cert không tin cậy | Không | Dialog trust cert |
| Out-of-date | commit trên base cũ | Không | Banner + Update |
| Lock | file bị người khác lock, token sai | Không | Dialog lock |
| WC busy | WC_LOCK bởi tool khác | Người dùng retry | "Đang được dùng bởi chương trình khác" |
| WC cần cleanup | work queue dở | Tự chạy work queue nếu không ai giữ lock | Banner Cleanup nếu thất bại |
| Định dạng | format < 31 hoặc > 32 | Không | Banner hướng dẫn |
| Đĩa | đầy, quyền, file bị giữ (Windows) | Retry rename 10 lần/≤2s (như svn) | Thông báo rõ đường dẫn |
| Bug engine | invariant vi phạm | Không | "Lỗi nội bộ" + tạo gói chẩn đoán; không ghi tiếp vào WC |

Request không idempotent (POST/PUT/MERGE/LOCK) không retry tự động; commit lỗi → xóa txn và báo.

### 3.3 Bảo mật
- TLS bắt buộc verify qua trust store OS; ngoại lệ chỉ bằng fingerprint người dùng chấp nhận.
- Credential chỉ nằm trong keyring; trong RAM dùng `secrecy`/`zeroize`.
- `tracing` có layer lọc header `Authorization`, URL có userinfo.
- Không telemetry ra ngoài; "Report a problem" chỉ tạo file zip cục bộ.
- WebView: CSP chặt, không tải nội dung remote; Tauri capabilities giới hạn command theo cửa sổ.
- Nội dung file hiển thị trong diff được escape (không render HTML/SVG của repo).

### 3.4 Quan sát & chẩn đoán
- Log mức info mặc định; mỗi operation có `op_id` xuyên suốt request HTTP.
- Đo thời gian từng request (method, path rút gọn, status, bytes, ms) mức debug.
- "Gói chẩn đoán": log 3 ngày gần nhất, phiên bản app/OS, cấu hình (không secret), `wc.db` schema + thống kê (không nội dung file).
- Crash: panic hook ghi stack vào log + hiện dialog khi mở lại.

## 4. Quy mô & hiệu năng

### 4.1 Ước lượng tải (1 người dùng)
| Kịch bản | Con số | Hệ quả thiết kế |
|---|---|---|
| WC lớn | 100k file, 50GB, 20k thư mục | Quét song song (`ignore` WalkParallel), so stat trước hash |
| Pristine | format 31: +50GB; format 32: chỉ file đã sửa | Sau MVP: gợi ý format 32 khi repo > 10GB binary |
| Update lớn | 10k file đổi, 5GB | 4–16 kết nối song song, stream ghi đĩa, tiến trình theo bytes |
| Log | 1M revision | Phân trang 100, cache, tìm kiếm trong cache local + server |
| Watcher | 20k thư mục | Linux inotify cần `max_user_watches` ≥ 65536 → cảnh báo; fallback quét định kỳ |

### 4.2 Tải lên server
- Poller: 1 request nhẹ / WC / 10 phút; 500 người dùng → < 1 req/s — không đáng kể.
- Giới hạn kết nối song song mặc định 4 để không quá tải VisualSVN; cấu hình được.

## 5. Kiểm thử & chất lượng
| Lớp | Cách |
|---|---|
| sans-IO protocol | Fixture XML thật (bắt từ svn CLI ↔ VisualSVN), snapshot `insta` |
| svndiff, XML parser | `cargo-fuzz` |
| wc.db | Compat hai chiều với svn 1.14/1.15 trong Docker; so dump DB |
| Crash safety | Kill ngẫu nhiên giữa update/commit (1000 vòng/đêm) |
| Hiệu năng | Benchmark repo tổng hợp 100k file, so svn CLI; chạy nightly, cảnh báo khi chậm > 10% |
| UI | Vitest component; E2E `tauri-driver` Windows/Linux cho luồng 10.1–10.5 |

## 6. Trade-off & xem lại khi lớn lên
| Quyết định | Đánh đổi | Xem lại khi |
|---|---|---|
| Single process (UI + engine) | Đơn giản, không IPC giữa process; panic engine có thể kéo sập app (giảm thiểu: `catch_unwind` biên command) | Cần Explorer/Finder integration → tách daemon dùng chung |
| WcActor tuần tự cho lệnh ghi | An toàn, dễ suy luận; không update 2 thư mục con song song | Người dùng cần thao tác song song trong cùng WC |
| Cache SQLite cục bộ, không server phụ | Không cần hạ tầng | Muốn tìm kiếm log toàn công ty / thống kê → dịch vụ index riêng |
| Polling incoming | Đơn giản | VisualSVN có webhook/push → chuyển sang push |
| Chỉ HTTP(S) | Giảm phạm vi | Cần `svn://`/`svn+ssh://` |
