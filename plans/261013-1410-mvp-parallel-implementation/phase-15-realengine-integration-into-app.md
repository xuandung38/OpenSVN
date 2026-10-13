---
phase: 15
title: "RealEngine integration into app"
status: pending
priority: P1
effort: "15a: 1–1.5 tuần (wave 2) · 15b: 2 tuần (wave 3) · 1 agent (nên là agent của P08)"
dependencies: [8, 10, 11]
wave: 2-3
owner_paths: ["apps/opensvn/src-tauri/src/engine/real/**", "tests/compat/scenarios/e2e/engine/"]
---

# Phase 15: RealEngine integration into app

## Overview
Nối engine thật vào `EngineFacade` của app (P08), thay `FakeEngine` mà không đổi IPC. Tách 2 bước bám theo mốc:
- **15a** (sau 10a): command remote (repo_info, list, cat, log, blame, lock/unlock, branches_list, layout_detect) → **M1 Read-only client**.
- **15b** (sau 11U/11C): command working copy (checkout, status, diff, commit, update, switch, revert, add/delete/move, cleanup, set_depth, resolve, ignore, props) → **M2/M3**.

## Requirements
- `RealEngine` implement `EngineFacade` (trait do P08 định nghĩa); cùng bộ test tích hợp với `FakeEngine` (chạy cả 2 impl).
- Tạo `SvnClient` với ctx: `AuthPrompter` (AuthBroker P08), `LockOwnershipStore` (app.sqlite P08), `state_dir` = thư mục dữ liệu app/<wc-id> (commit journal), config (`settings` + `~/.subversion/config`), progress → `op://progress`, cancel → `CancelToken`.
- Map `WcStatus` → `FileStatus`, `SvnError` → `AppError` (dùng mapping của `opensvn-ipc`, không tự viết lại).
- **Phục hồi khi mở app** (theo thứ tự, trước lệnh ghi đầu tiên mỗi WC):
  1. Gỡ WC_LOCK do tiến trình app đã chết để lại (`owned_by_dead_process`); lock lạ → banner "đang được dùng bởi chương trình khác" + nút gỡ có xác nhận.
  2. Chạy work queue còn dở (svn-wc).
  3. Commit journal: có txn chưa MERGE → `abort_txn`; đã MERGE nhưng WC chưa finalize → `svn-client` finalize theo journal.
- `ensure_authenticated` trước khi gửi lệnh ghi vào WcActor (không prompt khi đang giữ WC_LOCK).
- Đọc wc.db đi qua WcActor (rule từ ghi chú `svn_sqlite.c`); không mở connection wc.db ngoài engine.

## Architecture
```
apps/opensvn/src-tauri/src/engine/real/
  mod.rs           RealEngine: EngineFacade
  remote.rs        (15a) map command remote → svn-ra
  wc.rs            (15b) map command WC → svn-client
  recovery.rs      (15b) lock sidecar, work queue, commit journal
  mapping.rs       WcStatus/LogEntry/DirEntry → IPC types
```

## Related Code Files
- Create: như trên. Modify: không (P08 đăng ký `RealEngine` qua feature `real-engine` đã có sẵn từ P08).

## Implementation Steps
1. 15a: remote command + test tích hợp chạy với server Docker P06 → cùng 10a/12a phát hành M1 (P14 build).
2. 15b: WC command + progress/cancel + recovery.
3. Bật `real-engine` mặc định; FakeEngine giữ cho test FE.

## Success Criteria
- [ ] Bộ test tích hợp EngineFacade xanh cho cả FakeEngine và RealEngine.
- [ ] M1: Browse/History/blame/lock/tạo branch chạy với VisualSVN thật trên 3 OS.
- [ ] Kill app giữa commit/update → mở lại tự phục hồi (lock, work queue, journal) không cần thao tác tay.
- [ ] Hủy tác vụ engine thật < 1s (pha mạng).

## Risk Assessment
- Shape `SvnClient` lệch `EngineFacade` → 11U khóa chữ ký `SvnClient` bằng contract PR cuối wave 1; 15 review PR đó.
- Recovery chạy song song với TortoiseSVN đang mở WC → chỉ gỡ lock có trong sổ sở hữu của app.
