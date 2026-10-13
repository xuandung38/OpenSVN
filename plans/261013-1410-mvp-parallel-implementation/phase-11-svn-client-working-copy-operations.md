---
phase: 11
title: "svn-client working copy operations"
status: pending
priority: P1
effort: "8–9 tuần · 2 agent (track U: update/status, track C: commit/local ops)"
dependencies: [5, 7, 10]   # 07 mốc A2/B1/B2; 10b — trước đó dùng FakeRaSession
wave: 3
owner_paths: ["crates/svn-client/ (chia theo module, xem bảng)", "tests/compat/scenarios/client/"]
---

# Phase 11: svn-client working copy operations

## Overview
Lệnh cấp cao ghép `svn-wc` (P07) + `RaSession` (P10b) + `svn-diff` (P05): checkout, update, switch, status (local/remote), diff, commit, revert, add/delete/move, changelist, cleanup, lock/unlock, set-depth, resolve. API này là thứ `RealEngine` (P15) gọi.
Bắt đầu sớm (cuối wave 2) bằng `FakeRaSession` + `svn-wc` theo mốc A2/B1/B2; compat test cần 10b thật.
**Chữ ký `SvnClient` + `ClientCtx` khóa bằng contract PR của 11U ở cuối wave 1** (P15 review).

## Requirements
- Functional:
  - **checkout** (URL, rev, depth/sparse spec, ignore_externals; **luôn format 31**): tạo WC (P07) → `do_update` với editor `WcUpdateEditor` → work queue → xong.
  - **update/switch** (paths, rev, depth): reporter đọc WC (base rev, depth, start_empty, lock token, switched) → editor áp thay đổi. **Thứ tự nguyên tử bắt buộc cho mỗi file**: (1) nội dung mới → pristine tạm; (2) nếu có sửa local: `merge3` (P05) ghi kết quả/markers vào `.svn/tmp`; (3) gọi `install_updated_file` (P07) — đổi base + conflict + work item trong **một** transaction; (4) chạy work queue. Crash ở bất kỳ điểm nào → hoặc trạng thái cũ, hoặc trạng thái mới đầy đủ, **không bao giờ base mới + file làm việc dựa base cũ** (tránh revert ngầm thay đổi server). Tree conflict cơ bản (sửa local vs xóa server, xóa local vs sửa server) → ghi conflict, không mất dữ liệu; loại khác → lỗi rõ "dùng svn/TortoiseSVN để giải quyết"; obstruction → báo, không ghi đè.
  - **Xác thực trước khóa**: mọi lệnh ghi gọi `ra.ensure_authenticated()` **trước** khi lấy WC_LOCK; prompt không bao giờ xuất hiện khi đang giữ lock.
  - **Hủy 2 loại**: pha mạng → hủy ngay, `abort_edit`, thư mục còn `incomplete` (update lại tiếp tục); pha work queue → hủy giữa item, item lớn có checkpoint.
  - **status**: local (P07 M9) + remote (`do_update` dry-run với editor "status" chỉ đánh dấu out-of-date) + lock từ server; trả `FileStatus` (doc 03) gồm moved_from/to, changelist, needs_lock.
  - **diff**: base↔working (text, prop), rev↔rev (qua RA), wc↔rev; xử lý binary/too-large theo IPC.
  - **commit** (paths chọn, message, keep_locks, auto add/remove theo lựa chọn UI): thu thập committables (đệ quy cho thư mục add), kiểm conflict/out-of-date cục bộ, **drive `CommitEditor`** của RA (contract 01a; nội dung từ `ReplayableSource` = file làm việc đã detranslate vào `.svn/tmp`), **commit journal** trong `ctx.state_dir` (ghi committables + checksum trước `close_edit`; observer ghi txn name và rev mới); sau MERGE: finalize (cập nhật NODES base_rev/checksum/pristine, nhả lock, `bump_revisions`) rồi xóa journal. Mở lại sau crash: journal có rev → finalize; chỉ có txn → `abort_txn`. Lỗi trước MERGE → WC không đổi.
  - **Failpoint**: `commit.after_txn_created`, `commit.after_merge_before_finalize`, `update.after_merge3_before_txn`, `net.update.mid_stream` (P13).
  - **revert**, **add** (gồm auto-props từ `~/.subversion/config` và `svn:auto-props` kế thừa), **delete** (keep_local), **move/rename** (move tracking moved_to/moved_here), **changelist**, **mark_move** (missing + unversioned cùng nội dung → move).
  - **cleanup**: chạy work queue, gỡ WC_LOCK của chính app, tùy chọn xóa unversioned/ignored, vacuum pristine.
  - **lock/unlock** (gồm needs-lock: đổi quyền ghi file), **set_depth**/exclude, **resolve** (mine/theirs/working/base). (`upgrade_format`: ngoài MVP; WC format 32 chỉ đọc.)
  - **ignore**: `svn:ignore` (thay đổi prop, cần commit) và global-ignores cục bộ.
  - Callback progress (`Progress` doc 03) + cancel ở điểm an toàn (giữa các work item).
- Non-functional: mọi lệnh ghi lấy WC_LOCK; không giữ write txn SQLite qua lời gọi mạng; kill giữa chừng → mở lại work queue hoàn tất.

## Architecture — 2 track
| Module | Track | File |
|---|---|---|
| `checkout.rs`, `update/{reporter.rs, editor.rs, merge_file.rs, tree_conflict.rs}`, `switch.rs` | U | |
| `status/{local.rs, remote.rs}`, `diff.rs`, `set_depth.rs` | U | |
| `commit/{collect.rs, drive.rs, journal.rs, finalize.rs}`, `recovery.rs` | C | |
| `revert.rs`, `add.rs` (+auto-props), `delete.rs`, `move.rs`, `changelist.rs`, `ignore.rs`, `cleanup.rs`, `lock.rs`, `resolve.rs` | C | |
| `lib.rs` (facade `SvnClient`), `ctx.rs` (config, progress, cancel, auth), `config.rs` (đọc `~/.subversion/config`) | U sở hữu; C thêm hàm qua PR nhỏ | |

## Related Code Files
- Create: như bảng; `tests/compat/scenarios/client/*.yaml` (kịch bản luồng 10.1–10.5 doc 01 + edge case).
- Không sửa app: P15 nối `SvnClient` vào `RealEngine`.

## Implementation Steps
1. Ctx + config + facade (U) — khung chung trong 3 ngày để C bắt đầu.
2. U: reporter + update editor cho checkout sạch → compat: checkout fixture so `svn checkout` (cây file + `svn status` sạch bằng svn 1.15 trên WC engine tạo).
3. C: add/delete/revert/changelist/ignore trên WC fixture (không mạng).
4. U: update có thay đổi server (không sửa local) → update có sửa local (merge sạch) → conflict text → tree conflict cơ bản.
5. C: commit collect + driver + finalize; compat: engine commit → svn thấy đúng; svn commit → engine update đúng.
6. U: status remote, diff 3 dạng, switch, set_depth.
7. C: move/rename, mark_move, lock/unlock + needs-lock, cleanup, resolve.
8. Compat hai chiều đầy đủ + kịch bản TortoiseSVN thủ công (checklist Windows).
9. Bàn giao API cho P15; thêm failpoint theo danh sách P13.

## Success Criteria
- [ ] Mọi lệnh trên có compat test (server + svn CLI 1.14 & 1.15) xanh, gồm luồng 10.1–10.5.
- [ ] WC engine tạo/sửa: svn 1.14/1.15 `status/update/commit/cleanup` không lỗi; TortoiseSVN mở và commit được (checklist).
- [ ] Update có sửa local không bao giờ mất thay đổi (property test: random edit local + server → nội dung local còn trong file hoặc `.mine`).
- [ ] Commit lỗi giữa chừng → WC không đổi, server không có revision mới; crash sau MERGE → mở lại finalize đúng (không còn file "A" giả).
- [ ] Failpoint `update.after_merge3_before_txn` → mở lại: không có thay đổi server bị đảo ngược trong `svn diff`.

## Risk Assessment
- Tree conflict nhiều biến thể → MVP chỉ 4 kịch bản cơ bản; còn lại báo lỗi rõ "dùng svn/TortoiseSVN để giải quyết" (không đoán).
- 2 track cùng crate → facade do U sở hữu; rebase hằng ngày.
- Phụ thuộc P07 chưa xong → dùng API đã merge, phần thiếu stub `unimplemented` có test `#[ignore]` đánh dấu.
