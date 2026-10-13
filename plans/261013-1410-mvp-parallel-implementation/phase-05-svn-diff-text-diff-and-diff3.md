---
phase: 5
title: "svn-diff text diff and diff3"
status: pending
priority: P2
effort: "2.5–3 tuần · 1 agent (gồm port LCS libsvn_diff)"
dependencies: [1]   # cần gói 01a
wave: 1
owner_paths: ["crates/svn-diff/"]
---

# Phase 5: svn-diff text diff and diff3

## Overview
Diff 2 chiều (hiển thị, unified patch) và merge 3 chiều (diff3) có **hành vi giống libsvn_diff**: dùng khi update gặp sửa đổi local (P11) và cho conflict resolver (P12). Kèm phát hiện binary và chuẩn hóa EOL/whitespace.

## Requirements
- Functional:
  - `diff(a, b, opts) -> Vec<Hunk>` cho **hiển thị** (opts: ignore_space none|change|all, ignore_eol_style, context lines) dựa `imara-diff` (histogram).
  - **LCS port từ `libsvn_diff`** (lcs.c, diff.c, diff3.c — Apache 2.0) dùng cho `merge3` và `unified` để kết quả giống svn.
  - `unified(...) -> String` định dạng giống `svn diff` (header `Index:`, `===`, `--- a (revision N)`, `+++ b (working copy)`, `\ No newline at end of file`).
  - `merge3(base, mine, theirs, opts) -> MergeResult { text, conflicts: Vec<ConflictRegion> }` với marker kiểu svn: `<<<<<<< .mine`, `||||||| .rN`, `=======`, `>>>>>>> .rM` (tên nhãn cấu hình được), style: `modified_latest`, `only_conflicts`.
  - `is_binary(bytes, mime: Option<&str>)` theo quy tắc svn (`svn:mime-type` không bắt đầu `text/` → binary; heuristic NUL/tỉ lệ byte lạ cho file chưa có mime).
  - Nhận diện encoding hiển thị (UTF-8/UTF-16 BOM/Latin-1 fallback) cho UI — chỉ phục vụ hiển thị, merge làm trên byte.
- Non-functional: diff file 20k dòng < 50ms; merge3 file 100k dòng < 300ms; xử lý byte (không yêu cầu UTF-8).

## Architecture
```
crates/svn-diff/src/
  lib.rs      api
  tokens.rs   tách dòng theo byte, giữ EOL; chuẩn hóa theo opts để so sánh
  display.rs  hunks hiển thị từ imara-diff
  lcs.rs      port LCS libsvn_diff (dùng cho merge3/unified)
  unified.rs  formatter giống svn diff
  diff3.rs    thuật toán diff3 (base↔mine, base↔theirs, gộp vùng) theo semantics libsvn_diff/diff3.c
  binary.rs   phát hiện binary/encoding
tests/golden/ (input + expected sinh bằng svn 1.15 `svn diff`, `svn merge` / `svn update` conflict)
```

## Related Code Files
- Create: các file trên, `benches/`.

## Implementation Steps
1. Tokenizer dòng + opts ignore space/eol; property test.
2. Port LCS libsvn_diff + unified formatter; golden so với `svn diff` (bỏ header ngày). Diff hiển thị (imara) chỉ kiểm đúng nội dung, không kiểm ranh giới hunk.
3. diff3: cài theo tham chiếu `libsvn_diff/diff3.c` (Apache 2.0); xử lý cả thay đổi giống nhau 2 phía (không conflict), sửa liền kề, xóa cả file.
4. Marker conflict + style; golden với kịch bản `svn update` tạo conflict thật (P06 cung cấp tool sinh; trước đó tự sinh bằng svn local).
5. binary/encoding.
6. Bench.

## Success Criteria
- [ ] ≥ 40 golden case `unified` + ≥ 40 golden case `merge3` (dùng LCS port) khớp byte-for-byte với svn 1.15.0; diff hiển thị: nội dung đúng 100% (property test).
- [ ] Property test: merge3(base, x, base) == x; merge3(base, base, y) == y; merge3(b, x, x) == x không conflict.
- [ ] Bench đạt ngưỡng trên.

## Risk Assessment
- Khác biệt nhỏ với libsvn_diff (vị trí ranh giới hunk) gây kết quả merge khác svn → golden test lớn, ưu tiên đúng hơn nhanh.
- Port LCS tốn công hơn dùng imara → ước lượng đã gồm; imara chỉ cho UI.
