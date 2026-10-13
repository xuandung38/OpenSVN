---
phase: 7
title: "svn-wc database and pristine store"
status: pending
priority: P1
effort: "8–9 tuần · 2 agent (track A + track B), merge trung gian theo mốc A1/A2/B1/B2"
dependencies: [1]   # cần gói 01a + ghi chú svn_sqlite.c
wave: 1
owner_paths: ["crates/svn-wc/ (chia theo module, xem bảng)", "tests/compat/scenarios/wc/"]
---

# Phase 7: svn-wc database and pristine store

## Overview
Đọc/ghi `.svn/` **đúng chuẩn libsvn_wc format 31 & 32** (ADR-004): schema SQLite, mô hình NODES nhiều lớp (op_depth), pristine store, work queue, conflict skel, WC_LOCK, translation EOL/keywords, cache status. Không làm mạng — P11 ghép với RA.
Tham chiếu chính: [reports/from-researcher-to-planner-libsvn-wc-db-format-report.md](./reports/from-researcher-to-planner-libsvn-wc-db-format-report.md) (§1–§9).
**Bước 0 bắt buộc:** đợt research thứ 2 cho các mục UNVERIFIED (subst.c, skel.c, workqueue builders đầy đủ, conflicts.c grammar, đối chiếu tag **1.14.5 / 1.15.0**) → cập nhật report trước khi code M5/M6/M7. Journal mode/busy timeout lấy từ ghi chú `svn_sqlite.c` của gói 01a.
**Phạm vi MVP: đọc format 31 và 32; chỉ TẠO/GHI format 31.** WC format 32 (do svn 1.15 tạo) mở được để đọc/status/diff; lệnh ghi trên WC format 32 → lỗi rõ "format 32 chưa hỗ trợ ghi trong bản này". Ghi format 32, hydrate/dehydrate, `upgrade_format`: plan sau MVP.

## Requirements
- Functional:
  - Mở WC: tìm wcroot đi lên cây thư mục; đọc `PRAGMA user_version` ∈ {31, 32}; <31 → `WcFormatUnsupported` (hướng dẫn upgrade bằng svn CLI); >32 → từ chối.
  - Tạo WC mới **format 31**; schema/trigger/index/`sqlite_stat1` **tương đương cấu trúc** với svn tạo (so `sqlite_master` đã parse: bảng, cột, kiểu, ràng buộc, index, trigger — không so text SQL).
  - **Không bao giờ nâng 31 → 32; không bao giờ đổi journal mode / page settings của wc.db** (giữ như svn để TortoiseSVN dùng chung).
  - API mức WC cho P11 (đặt trong `svn_wc::db`):
    - đọc: `read_node(relpath)` (NODES_CURRENT), `read_base`, `children`, `actual_props`, `pristine_props`, `changelist`, `conflict`, `lock`, `externals`, `wc_lock_held`.
    - ghi (trong transaction + work item cùng txn): `base_add_dir/file/symlink`, `base_remove`, `op_add`, `op_delete`, `op_copy`, `op_move`, `op_revert`, `set_props`, `set_changelist`, `mark_conflict`, `resolve`, `set_lock`, `record_fileinfo`, `bump_revisions` (sau commit), `set_depth`/exclude.
  - Pristine: `install(stream) -> (sha1, md5, size)` (temp `.svn/tmp` → verify → rename vào `pristine/xx/<sha1>.svn-base`), `open(sha1)`, refcount do trigger (ACTUAL_NODE checksum xử lý tay), GC refcount=0 khi cleanup; format 32: chỉ **đọc** cột `hydrated` (pristine không có → diff/revert báo cần mạng, P11 lấy qua RA).
  - Work queue: encode/decode skel; runner thực thi đủ item MUST (file-install, file-remove, file-move, file-translate/file-copy-translated, sync-file-flags, prej-install, dir-install, dir-remove, base-remove, record-fileinfo; file-commit cho commit); chạy khi mở WC trước mọi lệnh ghi; item lạ → dừng, báo cần svn cleanup.
  - WC_LOCK: obtain/release theo thư mục + `locked_levels`; mỗi obtain/release ghi/xóa ở **`LockOwnershipStore`** (contract 01a, app.sqlite) cùng pid + boot_id; `cleanup` gỡ lock thuộc tiến trình app đã chết; lock không có trong sổ (TortoiseSVN/svn) → `WcBusy`, chỉ gỡ khi người dùng xác nhận (steal); dọn orphan.
  - Translation: eol-style (native/LF/CRLF/CR), keywords (Date, Revision, Author, HeadURL, Id, Header + dạng viết tắt, fixed-length `$Rev:: $`), special (symlink ↔ "link target"), executable (POSIX), needs-lock (read-only khi chưa lock) — byte-for-byte như svn.
  - Status local: so stat với `translated_size`/`last_mod_time` → nhanh; khác → so nội dung (detranslate working vs pristine); cập nhật cache khi chưa sửa (như svn).
  - **API update nguyên tử cho P11**: `install_updated_file(path, new_base: NodeBaseInfo, new_pristine: Sha1, working: WorkingResult)` với `WorkingResult = Install{from_tmp} | Merged{tmp} | Conflict{merged_tmp, markers, conflict_skel}` — đổi NODES base + ghi ACTUAL conflict + thêm work item cài file/marker **trong MỘT transaction**; file tạm phải đã nằm trong `.svn/tmp` trước khi gọi.
  - **Editor support**: `incomplete` presence cho thư mục đang update (abort_edit/hủy → thư mục còn `incomplete`, update lại tiếp tục được); chấp nhận nhiều file mở cùng lúc (close trễ).
  - **Failpoint** (feature `failpoints`, crate `fail`): `wc.txn.before_commit`, `wc.txn.after_commit`, `wq.mid_item`, `wq.rename`, `pristine.install.before_rename` (P13 dùng).
  - Đọc wc.db: dùng busy timeout/locking như ghi chú `svn_sqlite.c`; mọi truy cập đi qua `WcDb` (một connection ghi/WC, đọc có busy-retry); không giữ write txn qua lời gọi mạng.
  - Conflict: đọc/ghi conflict skel (text/prop/tree, operation update/switch/merge, version locations) + file marker `.mine/.rN/.rM`, `.prej`.
  - EXTERNALS, `dav_cache`, `inherited_props`: đọc/ghi đúng định dạng (P11 dùng tối thiểu ở MVP).
- Non-functional: SQLite qua `rusqlite` bundled, journal mode / busy timeout **giống svn** (ghi chú 01a); status 100k file lần sau < 3s (quét song song `ignore`).

## Architecture — module & chia 2 track
| Module | Nội dung | Track | File |
|---|---|---|---|
| M1 skel | codec skel `<len>:<bytes>`, list `( )` | A | `src/skel.rs` |
| M2 schema | tạo (31)/mở (31, 32 đọc), user_version, format detect | A | `src/db/schema.rs`, `src/db/sql/*.sql` |
| M3 nodes | mô hình lớp op_depth, NODES_CURRENT/BASE, moved_to/here, presence | A | `src/db/nodes.rs`, `src/db/ops.rs` |
| M8 wc_lock | WC_LOCK obtain/release/steal/orphan | A | `src/db/wc_lock.rs` |
| M10 externals | EXTERNALS, dav_cache, inherited_props, LOCK cache | A | `src/db/externals.rs`, `src/db/misc.rs` |
| M4 pristine | store, refcount, GC, đọc cờ hydrated | B | `src/pristine.rs` |
| M5 translate | eol/keywords/special/executable/needs-lock | B | `src/translate/*.rs` |
| M6 workqueue | item codec + runner | B | `src/workqueue/*.rs` |
| M7 conflicts | conflict skel + marker files | B | `src/conflicts.rs` |
| M9 status | quét FS song song + cache stat | B | `src/status.rs` |
| — | API công khai `WcDb` (facade) | A sở hữu, B thêm hàm qua PR nhỏ | `src/lib.rs`, `src/db/mod.rs` |

Thứ tự (report §8): A: M1 → M2 → M3, M8 → M10. B: M5 (thuần, bắt đầu ngay) → M4 (chờ M2) → M6, M7 (chờ M1, M4, M5) → M9 (chờ M3).

**Mốc merge trung gian** (mỗi mốc 1 PR, mở khóa P11 sớm):
| Mốc | Nội dung | Mở khóa |
|---|---|---|
| A1 (tuần ~3) | M1 skel + M2 schema đọc/tạo 31 | B-M4 |
| A2 (tuần ~6) | M3 nodes đọc + ghi op_* + M8 wc_lock + sổ sở hữu | 11U/11C (đọc/ghi metadata) |
| B1 (tuần ~4) | M5 translate + M4 pristine | 11U checkout |
| B2 (tuần ~7) | M6 work queue + M7 conflicts + API update nguyên tử | 11U update có sửa local |
| A3/B3 (tuần ~9) | M9 status, M10 externals (đọc), compat đầy đủ | 11 hoàn tất |

## Related Code Files
- Create: như bảng; `tests/fixtures_read.rs` (đọc mọi WC fixture của P06), `tests/roundtrip_compat.rs`, `tests/scenarios/` trong `tests/compat/scenarios/wc/`.

## Implementation Steps
1. **Bước 0** (cả 2 track, 3–4 ngày): research bổ sung mục UNVERIFIED; cập nhật report; chốt journal mode/busy timeout, grammar conflict skel, arity work item, quy tắc keyword.
2. A-M1 skel + test vector lấy từ wc.db thật (WORK_QUEUE, conflict_data, properties).
3. A-M2 schema: SQL theo `wc-metadata.sql` tag 1.14.5 (format 31; không copy block "format 99"/33); test so cấu trúc `sqlite_master` với WC svn 1.14.5 và 1.15.0 (format 31); mở được WC format 32 (đọc).
4. B-M5 translate: golden với file svn tạo (mọi eol-style, keyword) trên Linux và Windows (CRLF).
5. A-M3 nodes: đọc đúng mọi WC fixture (sạch, add, delete, copy, move, sparse, mixed-rev); ghi op_* → `svn status --xml` (svn 1.15 thật) khớp kỳ vọng.
6. B-M4 pristine + refcount; test GC không xóa pristine đang được ACTUAL_NODE tham chiếu.
7. A-M8 wc_lock + test 2 tiến trình (engine vs `svn` đang giữ lock).
8. B-M6 work queue: runner chạy được queue do svn để lại (fixture "kill -9 giữa update" từ P06).
9. B-M7 conflicts: đọc conflict do svn tạo; ghi conflict → `svn resolve --accept=working` (svn thật) chạy đúng; TortoiseSVN mở được (test tay Windows).
10. B-M9 status: bench 100k file; so kết quả với `svn status --xml`.
11. A-M10 externals/misc (**đọc** externals ở MVP; ghi externals ngoài MVP).
12. Compat hai chiều đầy đủ trong `tests/compat/scenarios/wc/`.

## Success Criteria
- [ ] Đọc đúng 100% WC fixture (≥ 25 trạng thái × svn 1.14/1.15) — kết quả status khớp `svn status --xml`.
- [ ] WC/thay đổi do engine ghi (format 31) → svn 1.14.5 & 1.15.0 `status`, `info`, `diff`, `update`, `commit`, `cleanup` chạy không lỗi, không báo hỏng.
- [ ] Cấu trúc `sqlite_master` + `PRAGMA user_version` (31) engine tạo tương đương svn tạo; journal mode không đổi sau khi engine ghi.
- [ ] `install_updated_file` nguyên tử: failpoint `wc.txn.before_commit`/`after_commit` không bao giờ để base mới + file làm việc cũ.
- [ ] Crash có lock: mở lại gỡ đúng lock của app đã chết; lock của svn CLI đang chạy không bị gỡ.
- [ ] Translation khớp byte-for-byte trên 3 OS.
- [ ] Work queue do svn để lại được hoàn tất; WC sau đó `svn status` sạch.
- [ ] Status 100k file lần sau < 3s (SSD).

## Risk Assessment
- Phần lớn rủi ro dự án nằm ở đây (semantics libsvn_wc). Giảm: bước 0 research, compat test mỗi PR, chỉ hỗ trợ đúng thao tác MVP cần, phần chưa hỗ trợ trả lỗi rõ thay vì đoán.
- 2 track cùng crate → ranh giới module rõ; API facade do A sở hữu, B mở PR nhỏ; rebase hằng ngày.
- Chuẩn hóa Unicode relpath (NFC/NFD) trên macOS chưa rõ → test riêng tên tiếng Việt có dấu trên APFS.
- Journal mode khác svn có thể hỏng truy cập đồng thời với TortoiseSVN → xác minh bước 0, test đồng thời.
