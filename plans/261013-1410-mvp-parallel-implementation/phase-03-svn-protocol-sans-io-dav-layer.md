---
phase: 3
title: "svn-protocol sans-IO DAV layer"
status: pending
priority: P1
effort: "3–4 tuần · 1 agent"
dependencies: [1]   # cần gói 01a
wave: 1
owner_paths: ["crates/svn-protocol/", "tests/compat/scenarios/protocol/"]
---

# Phase 3: svn-protocol sans-IO DAV layer

## Overview
Lớp giao thức **không làm I/O**: dựng request (method, URL, header, body stream) và parse response streaming cho toàn bộ thao tác HTTPv2 của SVN mà MVP cần. P10 ghép lớp này với transport (P04) thành `RaSession`.
Tham chiếu chính: [reports/from-researcher-to-planner-svn-http-protocol-v2-report.md](./reports/from-researcher-to-planner-svn-http-protocol-v2-report.md) §1–§5, §9.

## Requirements
- Functional (theo work package của report §7):
  - **xml**: helper quick-xml reader/writer namespace-aware, bỏ qua element lạ (forward-compat), escape/base64 cho prop không XML-safe.
  - **caps**: build `OPTIONS` (`D:options` + `D:activity-collection-set`); parse header `SVN-Me-Resource`, `SVN-Repository-Root/UUID`, `SVN-Youngest-Rev`, các stub (`SVN-Rev-Stub`, `SVN-Rev-Root-Stub`, `SVN-Txn-Stub`, `SVN-Txn-Root-Stub`, VTxn), `SVN-Supported-Posts`, `SVN-Allow-Bulk-Updates`, `SVN-Repository-MergeInfo`, token `DAV:` (depth, mergeinfo, log-revprops, inline-props, svndiff1, svndiff2, put-result-checksum, list…). **Không có `SVN-Me-Resource` → lỗi rõ "server không hỗ trợ HTTPv2" (MVP không làm DeltaV v1).** `UrlBuilder` dựng URL từ stub server trả, **không hard-code `!svn`**. `UrlBuilder::accept_href(href)` chỉ nhận href **cùng scheme/host/port với session và nằm dưới tiền tố stub** (`rvr`/`txr`/`me`/public path) — khác → `UnsafePath`. Mọi tên node/path trong response đi qua `RelPath::from_server`.
  - **propfind**: stat (Depth 0), list (Depth 1), get-lock; parse `D:multistatus` → `DirEntry`/props (ns svn/custom/dav).
  - **get**: request GET pegrev; response = byte stream thô (không parse).
  - **reports read-only**: `log-report` (mọi option ở report §2, phát `LogEntry` từng item), `file-revs-report` (phát `FileRev { path, rev, rev_props, prop_diffs, txdelta: stream base64 }`), `get-locks-report`. (`mergeinfo-report`: **ngoài MVP**.)
  - **update-report**: builder (src-path, target-revision, dst-path cho switch, update-target, depth, send-copyfrom-args, ignore-ancestry, `send-all` hay skelta + `include-props`), nhận reporter calls (`set_path/delete_path/link_path` → `S:entry`/`S:missing`); parser streaming → **sự kiện** `UpdateEvent` (OpenDir, AddDir, OpenFile, AddFile{checked_in_href}, DeleteEntry, Absent*, SetProp/RemoveProp, FetchProps, FetchFile{href}, TxDelta(chunk base64), CloseFile{checksum}, CloseDir, TargetRevision). P10 chuyển sự kiện thành lời gọi `DeltaEditor` + lên lịch GET.
  - **commit**: builder `POST me` (`create-txn` / `create-txn-with-props` skel), MKCOL, PUT (header `X-SVN-Base-Fulltext-MD5`, `X-SVN-Result-Fulltext-MD5`, `X-SVN-Version-Name`, Content-Type svndiff), DELETE, COPY (`Destination`, `Depth`), PROPPATCH (`D:propertyupdate`), header `If:` tagged-list cho lock token, `MERGE` body (`D:merge` + `S:lock-token-list`, `X-SVN-Options: release-locks|keep-locks`), parse `D:merge-response` (rev mới, ngày, author, `S:post-commit-err`), DELETE txn khi hủy.
  - **locks**: `LOCK` (`D:lockinfo`, `Timeout`, `X-SVN-Options: lock-steal`), parse `D:lockdiscovery`; `UNLOCK` (`Lock-Token`, `lock-break`).
  - **errors**: parse `D:error` / `m:human-readable@errcode` (chuỗi lồng) → `SvnError` với `ErrorCode::Svn(n)` + map sang code ngữ nghĩa (160042 → OutOfDate, 160035 → LockedByOther, 160013 → PathNotFound…); fallback HTTP status khi body không phải XML (trang lỗi proxy/IIS).
- Non-functional: parser streaming (bộ nhớ không phụ thuộc kích thước response); fuzz an toàn; không phụ thuộc tokio/reqwest (chỉ `bytes`, `quick-xml`, `base64`, `svn-types`, `svn-delta` cho skel/txdelta không cần).

## Architecture
```
crates/svn-protocol/src/
  lib.rs            HttpRequest{method, url, headers, body: Body}, Body = Empty | Bytes | Stream(Box<dyn ...>) ; trait ResponseParser { feed(&[u8]) -> Vec<Event>; finish() }
  xml/{reader.rs, writer.rs, ns.rs}
  caps.rs           Capabilities, UrlBuilder (me, rev, rvr, txn, txr từ stub)
  propfind.rs  get.rs  log.rs  file_revs.rs  locks.rs
  update/{request.rs, events.rs, parser.rs}
  commit/{request.rs, merge.rs, skel.rs}
  errors.rs         D:error parser + errcode table (svn_error_codes.h subset)
tests/data/<op>/  fixture tay tối thiểu; tests/golden.rs dùng thêm fixtures/protocol/ (P06)
fuzz/fuzz_targets/{update_parser, log_parser, multistatus, error_body}.rs
```

## Related Code Files
- Create: như trên; `tests/compat/scenarios/protocol/*.yaml` (kịch bản capture → parse → so với svn --xml).

## Implementation Steps
1. xml helpers + errors (bắt đầu ngày 1, nhỏ, phục vụ mọi thứ).
2. caps + UrlBuilder; golden từ capture OPTIONS (VisualSVN-like từ P06; trước đó dùng mẫu trong report).
3. propfind/get/locks.
4. log + get-locks; golden so với `svn log --xml -v`.
5. file-revs (phát txdelta base64 thô, không giải ở đây).
6. update-report builder + parser sự kiện; golden cho checkout, update có thay đổi, switch, status -u, cả **send-all** và **skelta**.
7. commit builders + merge-response; golden từ capture commit đầy đủ.
8. Fuzz 4 target (gồm attr `name`/`href` độc hại); snapshot test (insta) cho mọi builder (body XML ổn định).
9. Ghi bảng "element/headers đã xác minh" vào `crates/svn-protocol/PROTOCOL-NOTES.md` (đối chiếu tag release, không chỉ trunk — xem câu hỏi mở của report).

## Success Criteria
- [ ] Mọi builder có snapshot; mọi parser chạy golden từ capture thật (`fixtures/protocol/`) cho svn 1.14 và 1.15 server.
- [ ] update-report parser xử lý chunk cắt ở vị trí bất kỳ (property test) và cả hai chế độ send-all/skelta.
- [ ] Fuzz 1h/target không panic.
- [ ] Không dependency I/O (cargo tree kiểm trong CI — khả thi vì `svn-types` không có tokio).
- [ ] Href khác origin / ngoài stub, path chứa `..`/`.svn`/tên cấm bị từ chối (golden + fuzz).

## Risk Assessment
- Chi tiết chưa xác minh trong report (get-location-segments, Content-Type PUT, status lock conflict, header Negotiate) → chỉ chốt khi có capture thật từ P06; ghi rõ trong PROTOCOL-NOTES.
- Trunk khác tag release VisualSVN dùng → đối chiếu tag 1.14.x/1.15.x khi viết golden.
- Server dùng `SVNPathPrefix` khác `!svn` → UrlBuilder chỉ dùng stub.
