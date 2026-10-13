---
phase: 2
title: "svn-delta svndiff codec"
status: pending
priority: P1
effort: "1.5–2 tuần · 1 agent"
dependencies: [1]   # cần gói 01a
wave: 1
owner_paths: ["crates/svn-delta/"]
---

# Phase 2: svn-delta svndiff codec

## Overview
Thư viện thuần (không I/O mạng) encode/decode **svndiff 0/1/2** và áp delta window lên nguồn → đích dạng stream. Dùng bởi P03/P10 (update-report, commit PUT, file-revs) và P07/P11 (áp delta vào pristine).

## Requirements
- Functional:
  - Decoder streaming: nhận chunk byte bất kỳ → phát `TxDeltaWindow` (svn-types); hỗ trợ header `SVN\0`, `SVN\1` (zlib), `SVN\2` (lz4).
  - Applier: `apply(source: impl Read + Seek, windows) -> impl Write` (sview offset/len, op Source/Target/New), kiểm MD5 kết quả.
  - Encoder: **chỉ fulltext** → svndiff v0/v1 (op New, window ≤ 100KB) — đủ cho commit. Delta encoder so với base: **ngoài MVP**.
  - Varint (7-bit, MSB continuation) encode/decode.
- Non-functional: không nạp cả file vào RAM (window ≤ 100KB + view nguồn); ≥ 300 MB/s decode svndiff0 trên máy dev; pure Rust (`flate2` miniz_oxide, `lz4_flex`, `md-5`, `sha1`).

## Architecture
```
crates/svn-delta/src/
  lib.rs            pub use
  varint.rs         encode/decode số nguyên svndiff
  window.rs         parse/serialize 1 window (sview_off, sview_len, tview_len, ins_len, new_len, ins, new)
  decoder.rs        SvndiffDecoder: feed(&[u8]) -> Vec<TxDeltaWindow>; finish()
  encoder.rs        SvndiffEncoder::fulltext -> stream byte; version 0/1
  apply.rs          WindowApplier: impl svn_types::WindowSink, ghi đích + tính MD5/SHA1
  compress.rs       zlib/lz4 cho svndiff1/2 (header độ dài gốc varint)
  base64.rs         wrapper stream base64 (svndiff trong XML update-report)
tests/ fuzz/
```
Spec: `notes/svndiff` trong source Subversion + report `reports/from-researcher-to-planner-svn-http-protocol-v2-report.md`.

## Related Code Files
- Create: các file trên, `fuzz/fuzz_targets/{decode,roundtrip}.rs`, `benches/decode.rs` (criterion).

## Implementation Steps
1. varint + test biên (0, 127, 128, u64::MAX).
2. Parser window + serializer; test với vector tay theo spec.
3. Decoder streaming (state machine chịu chunk bị cắt bất kỳ chỗ nào — property test cắt ngẫu nhiên).
4. svndiff1/2 (decode): giải nén instruction + new-data. **Layout độ dài gốc v1/v2 trong notes mâu thuẫn với svndiff.c → theo code + golden thật** (report protocol §3).
5. Applier + checksum; lỗi rõ khi op vượt view (`ErrorCode::Corrupt`).
6. Encoder fulltext v0/v1.
7. Base64 streaming adapter.
8. Golden test: file `.svndiff` sinh bằng svn thật (P06 cung cấp qua `fixtures/svndiff/`; trong lúc chờ, tự sinh bằng `svnadmin dump --deltas` cục bộ).
9. cargo-fuzz 2 target, chạy 30 phút trong nightly (P13 gắn vào workflow).

## Success Criteria
- [ ] Roundtrip encode(v0/v1)→decode→apply đúng byte cho 10k input ngẫu nhiên (proptest); decode v2 đúng mọi golden.
- [ ] Giải đúng toàn bộ golden fixture từ svn 1.14/1.15.
- [ ] Fuzz 1h không panic/OOM; mọi input hỏng trả `SvnError` (không panic).
- [ ] Bench decode ≥ 300 MB/s (svndiff0), bộ nhớ đỉnh < 2 MB cho file 1 GB.

## Risk Assessment
- Sai chi tiết nén svndiff1/2 (độ dài gốc, ngưỡng nén) → golden test từ svn thật bắt buộc.
- Window tham chiếu Target (self-copy chồng lấn) dễ sai → test riêng trường hợp overlap.
