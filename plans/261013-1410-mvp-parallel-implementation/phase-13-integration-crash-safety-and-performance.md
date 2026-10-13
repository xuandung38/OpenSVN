---
phase: 13
title: "Integration crash safety and performance"
status: pending
priority: P1
effort: "3–4 tuần (bắt đầu crash/compat ngay khi 11 có checkout+commit) · 1 agent"
dependencies: [11, 12, 15]
wave: 3-4
owner_paths: ["tests/crash/", "benches/", ".github/workflows/nightly.yml"]
---

# Phase 13: Integration crash safety and performance

## Overview
Chứng minh tiêu chí MVP về **toàn vẹn dữ liệu, tương thích, bảo mật và hiệu năng**; dựng nightly; định tuyến lỗi về gói owner (mở issue, không sửa crate người khác). Đề xuất thay đổi `docs/` qua PR cho lead.

## Requirements
- **Crash suite** (2 cơ chế):
  - **Failpoint có tên** (crate `fail`, bật bằng feature `failpoints` trong 07/10/11): `net.update.mid_stream`, `wc.txn.before_commit`, `wc.txn.after_commit`, `wq.mid_item`, `wq.rename`, `pristine.install.before_rename`, `commit.after_txn_created`, `commit.after_merge_before_finalize`, `update.after_merge3_before_txn`. Báo cáo **độ phủ theo failpoint** mỗi đêm.
  - **SIGKILL ngẫu nhiên từ ngoài** (thời gian ngẫu nhiên) trong checkout/update (có/không sửa local)/commit/revert/switch.
  - Sau mỗi lần: mở lại (recovery P15/svn-client) → kiểm `svn status`/`svn diff` (svn 1.15.0 thật): 0 hỏng WC, 0 mất thay đổi local, **0 revert ngầm thay đổi server** (diff không chứa đảo ngược commit người khác).
  - Biến thể chạy qua **app thật** (tauri-driver, Linux) cho luồng commit/update.
  - 1000 vòng/đêm tổng.
- **Concurrency suite**: engine + `svn` CLI (đại diện TortoiseSVN) cùng WC → khóa đúng, không `SQLITE_BUSY` làm hỏng thao tác, không hỏng DB.
- **Compat full matrix**: kịch bản P06/07/10/11 × server 1.14.5/1.15.0 × client 1.14.5/1.15.0.
- **Bảo mật**: fixture "server/repo độc hại" (P06) — path traversal, `.svn/…`, symlink trung gian, tên cấm Windows, href khác origin, XSS trong message/tên file → không file nào ngoài wcroot bị chạm, không request ra origin khác, không script chạy; secret ngẫu nhiên không xuất hiện trong log/diagnostics zip.
- **Perf bench** (repo tổng hợp 100k file / 10GB, script P06): checkout, update 10k file, status lần đầu/lần sau, log 100k revision (cache), commit 1k file — so svn CLI 1.15.0; lưu lịch sử, cảnh báo khi chậm > 10%. Cổng MVP: **không chậm hơn svn CLI**, status lần sau < 3s.
- **Fuzz nightly**: svndiff, XML parser, skel codec (30 phút/target).
- **Soak 2h**: app + watcher + poller trên WC 100k file; tăng RAM < 10%.
- **Dogfood** 1–2 tuần dev nội bộ; triage qua "Report a problem".
- Non-functional: nightly < 3h; báo cáo artifact JSON/HTML trong GitHub Actions.

## Architecture
```
tests/crash/{harness.rs, failpoints.rs, sigkill.rs, verify.rs}   (binary test nhỏ gọi svn-client)
benches/{checkout.rs, status.rs, update.rs, log.rs, commit.rs, compare_svn.sh}
.github/workflows/nightly.yml   crash + compat matrix + security fixtures + bench + fuzz + soak
```

## Related Code Files
- Create: như trên. Failpoint được thêm vào 07/10/11 bởi chính owner (P13 gửi danh sách tên ở trên trong issue ngay khi bắt đầu).

## Implementation Steps
1. Gửi danh sách failpoint cho 07A/07B/10b/11U/11C (tuần đầu wave 3).
2. Harness + verify (dùng svn CLI thật làm oracle).
3. Failpoint suite + SIGKILL suite + biến thể app thật.
4. Concurrency suite.
5. Security suite (fixture P06).
6. Bench + lịch sử; nightly workflow (self-hosted runner nếu thiếu dung lượng).
7. Soak 2h; dogfood + báo cáo phát hành MVP.

## Success Criteria
- [ ] 100% failpoint trong danh sách được kích hoạt ≥ 50 lần/đêm; 0 hỏng WC, 0 mất thay đổi, 0 revert ngầm.
- [ ] Compat full matrix + security suite xanh.
- [ ] Bench: không chậm hơn svn CLI; status lần sau < 3s; app RAM < 400MB với WC 100k file.
- [ ] Soak 2h: tăng RAM < 10%.
- [ ] Dogfood: 0 lỗi mức chặn trong tuần cuối.

## Risk Assessment
- Runner GitHub thiếu dung lượng → self-hosted (máy Mac/Linux của team).
- Lỗi tìm muộn ở 07/11 → bắt đầu crash/compat khi 11 có checkout+commit, không chờ 12b.
