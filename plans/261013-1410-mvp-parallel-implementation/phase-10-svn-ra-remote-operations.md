---
phase: 10
title: "svn-ra remote operations"
status: pending
priority: P1
effort: "10a: 2–2.5 tuần · 10b: 3–4 tuần · 1 agent nối tiếp (hoặc 2 nếu cần nhanh M1)"
dependencies: [2, 3, 4]
wave: 2-3
owner_paths:
  10a: ["crates/svn-ra/src/ops/{mod,open,read,log,locks,file_revs,blame}.rs", "tests/compat/scenarios/ra/read/"]
  10b: ["crates/svn-ra/src/ops/{update,commit,remote_edit}/**", "tests/compat/scenarios/ra/write/"]
---

# Phase 10: svn-ra remote operations

## Overview
Ghép protocol (P03) + transport/auth (P04) + svndiff (P02) thành `HttpRaSession: svn_ra_api::RaSession`.
- **10a (đọc)** → mở khóa mốc **M1 Read-only client** (cùng 12a + 15a).
- **10b (ghi)**: update driver (update-report → `EditorWorker` → `DeltaEditor`) và commit editor (txn HTTPv2 → MERGE), thao tác trực tiếp server (`remote_edit`, `copy_location`).

## Requirements
### 10a — đọc
- `open(url)`: OPTIONS → capabilities; từ chối server không HTTPv2 (`ServerUnsupported`); `ensure_authenticated` (OPTIONS có auth trước khi app lấy WC_LOCK).
- `youngest_rev`, `check_path`, `stat`, `list`, `get_file` (stream), `log` (streaming, limit/paging/revprops), `get_locks`, `lock`/`unlock` (kết quả từng path), `file_revs` (txdelta base64 → svndiff → áp tuần tự).
- `blame`: từ `file_revs` + `svn_diff::diff2` (P05). **Không làm bản LCS dự phòng** — nếu P05 chưa xong, 10a chờ phần blame (các hàm khác không chờ).
- **Mọi path/href từ server**: path qua `RelPath::from_server`; href phải cùng origin với session và nằm dưới tiền tố stub (`rvr`/`txr`/`me`) — sai → `UnsafePath`, không gửi request (không lộ credential).
### 10b — ghi
- **do_update**: gửi report, parse sự kiện (P03), chuyển thành lệnh tới `EditorWorker`.
  - send-all: txdelta inline → `WindowSink`.
  - skelta: `FetchFile` → GET song song (≤ pool size); file đóng **trễ** theo quy tắc contract 01a (nhiều file mở cùng lúc; `close_directory` chỉ sau khi con đã close); dữ liệu GET ghi thẳng vào sink của editor (không qua file tạm thứ 3).
  - **Spike 2 ngày**: GET delta theo base (`X-SVN-VR-Base` — xác minh trong `libsvn_ra_serf/update.c` tag 1.15.0) để giảm bytes khi update; áp dụng nếu xác minh được.
  - Hủy: đang stream → dừng request, `abort_edit` ngay (editor đánh dấu `incomplete`), mục tiêu < 1s.
- **commit_editor**: POST create-txn (`create-txn-with-props`) → `observer.on_txn_created(txn)` → MKCOL/PUT/DELETE/COPY/PROPPATCH (PUT song song có giới hạn) → `close_edit` = MERGE (lock tokens, keep/release) → `observer.on_merged(info)`; lỗi trước MERGE → DELETE txn; `post-commit-err` → cảnh báo, không retry.
  - PUT dùng `ReplayableSource` → gửi lại được sau 401/reset; chọn svndiff v0 hoặc v1 theo capability (encoder fulltext, không delta).
- `abort_txn(name)`: dọn txn rò (P15 gọi khi mở app thấy commit journal có txn chưa MERGE).
- `remote_edit` / `copy_location` (Browse: new folder, upload, rename, delete, copy → branch/tag): dựng commit editor không cần WC.
- Phân loại lỗi đúng `ErrorCode` (OutOfDate 160042, LockedByOther 160035, PathNotFound 160013, Forbidden…).
- Non-functional: checkout fixture 1GB **không chậm hơn** `svn checkout` 1.15.0 (cùng server/máy); RAM phẳng; không để txn treo khi hủy.

## Architecture
```
crates/svn-ra/src/ops/
  mod.rs (HttpRaSession)   open.rs   read.rs   log.rs   locks.rs   file_revs.rs   blame.rs      ← 10a
  update/{driver.rs, fetch_scheduler.rs, event_bridge.rs}                                        ← 10b
  commit/{editor.rs, put.rs, merge.rs}   remote_edit.rs                                          ← 10b
```
```
REPORT update-report ─► stream ─► P03 parser ─► UpdateEvent ─► event_bridge ─► EditorWorker (mpsc 64) ─► DeltaEditor (spawn_blocking)
                                         FetchFile{href} ─► scheduler (≤ N GET) ─► WindowSink của file đó ─► close_file khi xong
```

## Related Code Files
- Create: như trên; `crates/svn-ra/tests/ops_*.rs` (wiremock + fixture P06), `tests/compat/scenarios/ra/{read,write}/`.
- Không sửa `transport/`, `auth/` (P04), `lib.rs` (khóa bởi 01a).

## Implementation Steps
1. 10a: open/read/log/locks/file_revs → compat so `svn info/list/log --xml/cat/blame --xml` (server 1.14.5 & 1.15.0).
2. 10a: kiểm href/path từ server + fixture "server độc hại" (P06) → không request ra origin khác.
3. 10a: kiểm thử thật VisualSVN (Basic + SSO) — checklist trong PR → **M1** cùng 12a/15a.
4. 10b: update driver send-all → skelta + scheduler + close trễ; `RecordingEditor` kiểm quy tắc thứ tự; checkout fixture → so `svn export`.
5. 10b: spike GET delta; switch / status -u qua cùng driver.
6. 10b: commit editor (add/modify/delete/copy/prop/lock), out-of-date muộn ở MERGE, abort → DELETE txn, 401 giữa PUT → gửi lại từ `ReplayableSource`.
7. 10b: remote_edit/copy_location.
8. Bench checkout so svn CLI (script; P13 đưa vào nightly).

## Success Criteria
- [ ] 10a: mọi hàm đọc có compat test xanh với server 1.14.5 và 1.15.0; href khác origin bị chặn (test).
- [ ] 10b: checkout/update/switch fixture → cây file khớp `svn export` 100%; `RecordingEditor` không vi phạm quy tắc.
- [ ] Commit (add/modify/delete/copy/prop/lock) → `svn log -v`/`svn cat` khớp; abort/hủy không để txn treo; observer nhận đúng txn/rev.
- [ ] Hủy checkout lớn khi đang stream: dừng < 1s, `update` lại tiếp tục được.
- [ ] Checkout không chậm hơn svn CLI 1.15.0.

## Risk Assessment
- Thứ tự editor khi GET song song → quy tắc close trễ trong contract + RecordingEditor.
- Server ép send-all hoặc bỏ qua `text-deltas no` → hỗ trợ cả hai đường.
- Chi tiết header/endpoint chưa xác minh (report §12) → chốt bằng capture P06 trước khi code phần đó.
