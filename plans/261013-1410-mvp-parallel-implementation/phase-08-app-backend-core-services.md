---
phase: 8
title: "App backend core services"
status: pending
priority: P1
effort: "3–4 tuần · 1 agent"
dependencies: [1]   # cần gói 01b
wave: 1
owner_paths: ["apps/opensvn/src-tauri/ (trừ src/engine/real/** — P15; tauri.bundle.conf.json, icons/ — P14)", "apps/opensvn/src/ipc/bindings.ts (qua generator)"]
---

# Phase 8: App backend core services

## Overview
Tầng ứng dụng Rust trong `src-tauri`: thực thi các command/event của [doc 03](../../docs/system-design/03-ipc-api-contract-and-data-model.md) trên **FakeEngine**; định nghĩa `EngineFacade`; bảo mật WebView/IPC. Nối engine thật là phase **P15** (không đổi IPC).

## Requirements
- Functional:
  - **OperationManager**: tạo `OpId` (UUID v7), phát `op://started|progress|finished|failed`, throttle progress ≤ 10/s/op, cancel qua `CancelToken`, giữ 50 tác vụ gần nhất (`op_list`).
  - **WcRegistry + WcActor**: mỗi WC một actor (tokio task + mpsc); lệnh ghi tuần tự; lệnh đọc **qua actor** với busy-retry (không giả định WAL cho wc.db — theo ghi chú `svn_sqlite.c`); trong lúc lệnh ghi dài chạy, status đọc trả cache + đánh dấu "đang cập nhật"; `catch_unwind` ở biên command.
  - **Hủy**: chuyển `CancelToken` xuống engine; FakeEngine mô phỏng 2 loại điểm hủy (pha mạng hủy ngay, pha work queue giữa item).
  - **StatusService**: watcher `notify` (debounce 300ms, gom thư mục bẩn) → gọi engine status theo thư mục → phát `wc://status-changed` dạng delta; cache snapshot vào `cache.sqlite` để mở WC hiện ngay.
  - **IncomingPoller**: mỗi `general.checkIntervalMin` với WC đang mở → `youngest_rev` → `log` khoảng mới → `wc://incoming`.
  - **AuthBroker**: implement `svn_ra_api::AuthPrompter` bằng `auth://prompt` (payload có OriginKey đầy đủ host:port) + chờ `auth_respond` (timeout 5 phút → hủy); credential qua `secure-store` theo OriginKey; danh sách cert tin cậy (host, port, sha256 leaf) trong `app.sqlite`. Bảo đảm **không prompt khi đang giữ WC_LOCK**: WcActor gọi `ensure_authenticated` trước khi nhận lệnh ghi.
  - **LockOwnershipStore** (contract 01a) trên `app.sqlite` (bảng `wc_lock_owner(wc_root, dir, pid, boot_id, acquired_at)`).
  - **AppDb**: `app.sqlite` + `cache.sqlite` (WAL được phép ở 2 DB này) theo schema doc 03 §6 (bảng `account` khóa theo origin), migration có version; thư mục dữ liệu theo OS. **Không mở trực tiếp `.svn/wc.db`** — chỉ qua engine.
  - **Settings**: `settings_get/set` (doc 03 §6.3), phát thay đổi cho service liên quan (poller interval, max connections).
  - **Layout**: `layout_detect` (tìm trunk/branches/tags trên đường dẫn, hỗ trợ `/{project}/trunk`), `layout_get/set` (mode auto|custom|none).
  - **OS integration**: `reveal_in_os`, `open_external` (editor/terminal/default app theo settings), notification OS, menu native (Tauri menu API) theo [doc 01 §8](../../docs/system-design/01-ui-screens-menus-and-interactions.md) — menu phát event `menu://<id>` cho FE.
  - **Diagnostics**: `tracing` + file log xoay vòng 7 ngày/100MB, dùng redaction layer của P04 (header denylist, URL userinfo, lock token); `diagnostics_export` tạo zip **và cho người dùng xem trước danh sách/nội dung** trước khi lưu; tùy chọn ẩn đường dẫn và commit message.
  - **Bảo mật WebView/IPC**: CSP chặt (`default-src 'self'`; `script-src 'self'` không `unsafe-inline`/`unsafe-eval`; `connect-src ipc: http://ipc.localhost`; `img-src 'self' asset: blob: data:`); `capabilities/` chỉ cho cửa sổ `main` đúng danh sách command doc 03; asset protocol scope chỉ thư mục tạm của app; không bật plugin shell/fs tổng quát. Setting chạy chương trình (editor/diff/merge tool ngoài, terminal) **chỉ đổi qua dialog chọn file native + xác nhận** — command `settings_set` từ chối các key này.
  - **Proxy**: mật khẩu proxy lưu trong `secure-store`, không trong settings JSON.
  - **Engine adapter**: trait `EngineFacade` (checkout/update/commit/status/diff/log/…) + `FakeEngine` (dùng ngay) + đăng ký `RealEngine` qua feature `real-engine` (impl do P15). Bộ test tích hợp EngineFacade viết chung để chạy cho cả 2 impl.
- Non-functional: không chặn main thread; mọi command dài trả `OperationHandle` < 50ms; bộ nhớ app khi idle < 150MB.

## Architecture
```
apps/opensvn/src-tauri/src/
  main.rs lib.rs            setup, đăng ký command/event, menu
  commands/{wc,status,ops,remote,system,settings,auth}.rs   (chữ ký do P01 tạo; P08 điền thân)
  ops/manager.rs            OperationManager
  wc/{registry.rs, actor.rs}
  status/{service.rs, watcher.rs}
  incoming.rs
  auth/broker.rs
  db/{app_db.rs, cache_db.rs, migrations/}
  settings.rs layout.rs os_integration.rs menu.rs diagnostics.rs
  engine/{facade.rs, fake.rs, real.rs}
tests/  (tauri::test mock runtime + FakeEngine)
```

## Related Code Files
- Modify: `apps/opensvn/src-tauri/**` (skeleton từ P01), `apps/opensvn/src/ipc/bindings.ts` chỉ qua `pnpm gen:ipc` (không sửa tay).
- Không sửa: crate engine, `apps/opensvn/src/**` khác.

## Implementation Steps
1. AppDb + migrations + settings; test trên thư mục tạm.
2. OperationManager + event; test throttle/cancel.
3. EngineFacade + FakeEngine (dựa FakeRaSession + WC giả trong bộ nhớ) đủ cho mọi command → FE (P09/P12) chạy end-to-end với dữ liệu giả.
4. WcRegistry/WcActor; test tuần tự ghi, song song đọc, panic không sập.
5. StatusService + watcher (Linux: cảnh báo `max_user_watches`); test bằng FS tạm.
6. AuthBroker (prompt + secure-store + trusted cert) — implement `svn_ra_api::AuthPrompter` (contract 01a); LockOwnershipStore trên app.sqlite.
7. IncomingPoller, layout, OS integration, menu native 3 OS, notification.
8. Diagnostics + log filter (test không có credential trong log).
9. CSP + capabilities + asset scope + test (gọi command ngoài allowlist bị từ chối; `settings_set` key chạy lệnh bị từ chối; asset ngoài scope bị từ chối).
10. Bàn giao `EngineFacade` + bộ test chung cho P15.

## Success Criteria
- [ ] Mọi command doc 03 §3 trả kết quả đúng kiểu với FakeEngine (test tích hợp mock runtime).
- [ ] Hủy tác vụ dừng trong < 1s; progress không vượt 10 event/s.
- [ ] Watcher phát delta < 1s sau khi sửa file (WC 10k file test).
- [ ] Menu native đúng cấu trúc doc 01 §8 trên macOS và Windows/Linux.
- [ ] Property test: secret ngẫu nhiên không có trong log/diagnostics zip; zip có bước xem trước.
- [ ] Test bảo mật IPC: CSP header, capability allowlist, asset scope, setting chạy lệnh chỉ qua dialog native.

## Risk Assessment
- Contract IPC thiếu → contract PR vào `opensvn-ipc` (P01 owner = lead).
- Watcher trên repo lớn tốn tài nguyên (inotify) → fallback quét định kỳ, cấu hình được.
- Chuyển FakeEngine → RealEngine lộ khác biệt hành vi → bộ test chung chạy cho cả 2 impl (P15).
