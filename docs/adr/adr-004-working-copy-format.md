# ADR-004: Định dạng working copy — tương thích svn chuẩn (format 31 + 32)

**Status:** Accepted
**Date:** 2026-10-13
**Deciders:** PO, Tech lead

## Context
- PO yêu cầu **không mất tương thích**: working copy do OpenSVN tạo phải dùng được bằng svn CLI, TortoiseSVN, SmartSVN và ngược lại.
- Repo rất lớn, nhiều binary → pristine (bản gốc mọi file trong `.svn/pristine`) làm gấp đôi dung lượng.
- Subversion 1.15 đã phát hành:
  - **Format 31** (từ 1.8): mặc định, client 1.8–1.15 đọc được, luôn lưu pristine.
  - **Format 32** (từ 1.15): hỗ trợ **Pristines On Demand** (`--store-pristine=no`) — giảm tới ~50% dung lượng; chỉ client ≥1.15 đọc được (client cũ báo E155021).
  - Chế độ pristine thể hiện qua `svn info` ("Working Copy Store Pristine").

## Decision

> **Cập nhật 2026-10-13 (PO, red-team plan MVP):** MVP **đọc** format 31 và 32 nhưng **chỉ tạo/ghi format 31**. Lý do: svn 1.15.0 mới phát hành 2026-10-08, VisualSVN/TortoiseSVN có thể chưa hỗ trợ; giảm ~3 tuần đường găng. Format 32 (ghi, pristines-on-demand, upgrade) → plan sau MVP.

Engine đọc/ghi **đúng định dạng `.svn/wc.db` của libsvn_wc**:
1. **Format 31** — mặc định, tương thích rộng nhất (1.8+).
2. **Format 32** — tùy chọn khi checkout, cho phép `store-pristine=no` với repo nhiều binary (yêu cầu mọi tool còn lại của người dùng ≥1.15).
3. Không tạo format riêng. Dữ liệu riêng của app (cache log, cấu hình layout, shelve) lưu **ngoài** working copy, trong thư mục dữ liệu ứng dụng.

## Options Considered

### Option A: Format riêng `.svnx/`
**Pros:** tối ưu tự do. **Cons:** khóa người dùng vào app, mất tương thích — **bị PO loại**.

### Option B: Tương thích format 31 + 32 (chọn)
| Dimension | Assessment |
|---|---|
| Complexity | High — phải bám schema & semantics libsvn_wc |
| Cost | Cao hơn A ở phase 3 |
| Scalability | Format 32 giải quyết vấn đề dung lượng binary |
| Tương thích | Dùng chung với svn CLI / TortoiseSVN / SmartSVN |

**Pros:** người dùng chuyển dần, luôn có "lối thoát" bằng tool khác; tối ưu dung lượng có sẵn trong chuẩn.
**Cons:** phải cài đúng chi tiết: bảng `NODES` (op_depth), `ACTUAL_NODE`, `PRISTINE` (refcount, MD5/SHA-1), `WORK_QUEUE`, `WC_LOCK`, `LOCK`, `EXTERNALS`, conflict skel, `wcprops`; không thêm bảng/cột riêng.

## Trade-off Analysis
Tương thích là yêu cầu cứng. Tối ưu vẫn làm được ở tầng **ngoài định dạng**: tải song song, status nhanh nhờ cache stat trong RAM + FS watcher, streaming checkout — không đổi schema.

## Consequences
- Dễ hơn: test differential trực tiếp — engine ghi WC rồi `svn status/info` thật đọc lại (và ngược lại).
- Khó hơn: phải xử lý WC do tool khác để lại (work queue dở, lock) → chạy work queue hoặc yêu cầu cleanup giống svn.
- Chạy đồng thời với TortoiseSVN: tôn trọng `WC_LOCK` + khóa SQLite, không giữ transaction lâu.
- Xem lại: khi Subversion ra format mới (1.16+).

## Action Items
1. [ ] Nghiên cứu `subversion/libsvn_wc/wc-metadata.sql`, `wc-queries.sql`, `wc_db.c`, `workqueue.c`, `conflicts.c` (Apache 2.0) → viết đặc tả nội bộ `docs/wc-db-format-notes.md`.
2. [ ] Bộ test tương thích hai chiều: engine ↔ svn 1.14 (format 31) và svn 1.15 (format 31/32).
3. [ ] (Sau MVP) UI checkout: tùy chọn "Không lưu bản gốc (cần svn ≥1.15)" + cảnh báo tương thích.
