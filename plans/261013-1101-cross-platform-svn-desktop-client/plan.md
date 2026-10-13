# Plan: OpenSVN — SVN client desktop kiểu GitHub Desktop, engine Rust tự viết

Status: draft — chờ duyệt
Nghiên cứu: [../reports/research-261013-1101-smartsvn-github-desktop-style-svn-client-report.md](../reports/research-261013-1101-smartsvn-github-desktop-style-svn-client-report.md)
Kiến trúc & ADR: [../../docs/system-architecture.md](../../docs/system-architecture.md)

## Bối cảnh đã chốt
- Nền tảng: **Windows, Linux, macOS**. iPadOS (TestFlight) / Android để sau.
- Server: VisualSVN Server (HTTPS), tài khoản AD.
- Repo rất lớn, nhiều binary. Dùng nội bộ. Chưa cần JIRA.
- Tự viết engine SVN bằng Rust, không dùng svn CLI/libsvn.
- **Working copy tương thích svn chuẩn** (format 31 + 32) — dùng chung với TortoiseSVN/svn CLI.
- Team mạnh Rust + React. Repo/CI: GitHub Actions. Máy dev: macOS.

## Quyết định chính
| Hạng mục | Chọn | ADR |
|---|---|---|
| Engine | Rust, pure Rust ưu tiên, kiến trúc sans-IO | 001, 003 |
| UI | Tauri 2 + React 19/TS, CodeMirror 6, TanStack Virtual; dự phòng Electron | 002 |
| Giao thức | Chỉ HTTP(S) WebDAV/HTTPv2 (VisualSVN) | 003 |
| TLS | rustls + rustls-platform-verifier (CA nội bộ từ trust store OS) | 003 |
| Auth | Basic + Negotiate (SSPI Windows SSO / GSSAPI macOS-Linux) | 003 |
| Working copy | `.svn/wc.db` format 31; format 32 + pristines-on-demand cho repo binary | 004 |
| Secret | keyring v4 (Credential Manager / Keychain / Secret Service) | 003 |

## Crates
```
crates/svn-protocol   DAV/HTTPv2 request/response (sans-IO), XML quick-xml
crates/svn-delta      svndiff0/1/2 (zlib, lz4), delta editor
crates/svn-ra         RA API + transport reqwest/rustls, auth Basic/Negotiate
crates/svn-wc         wc.db chuẩn libsvn_wc, pristine store, work queue, translate
crates/svn-diff       diff (imara-diff), diff3, mergeinfo
crates/svn-client     checkout/update/commit/revert/switch/merge
apps/opensvn          Tauri 2 (src-tauri) + React (src)
tests/compat          test tương thích với svn CLI 1.14/1.15 (Docker)
```

## Phases
| # | Phase | Ước lượng | Phụ thuộc |
|---|---|---|---|
| 1 | [Nền móng giao thức & test harness](phase-01-protocol-foundation-and-test-harness.md) | 3–4 tuần | — |
| 2 | [Remote ops (không cần WC)](phase-02-remote-operations-without-working-copy.md) | 3–5 tuần | 1 |
| 3 | [Working copy engine tương thích wc.db](phase-03-working-copy-engine.md) | 12–16 tuần | 2 |
| 4 | [Desktop app MVP](phase-04-desktop-client-app-mvp.md) | 6–8 tuần (FE song song từ phase 2) | 2, 3 |
| 5 | [Repo lớn & binary](phase-05-large-repo-and-binary-optimization.md) | 4–6 tuần | 3 |
| 6 | [Merge, conflict, revision graph](phase-06-merge-conflicts-revision-graph-kerberos.md) | 10–14 tuần | 3 |
| — | Mobile (iPadOS/Android) | lập plan riêng sau | 4 |

Tới MVP dùng hằng ngày (1–4): ~6–7 tháng. Phase 3 dài hơn bản trước vì phải bám đúng semantics libsvn_wc.

## Tiêu chí chấp nhận tổng
- **Tương thích hai chiều**: WC do OpenSVN tạo/sửa → svn CLI 1.14/1.15 và TortoiseSVN `status`/`update`/`commit` bình thường, không yêu cầu cleanup; và ngược lại.
- Kết quả lệnh MVP khớp svn CLI trên bộ test compat (nội dung, props, revision, status).
- Windows join domain đăng nhập SSO không nhập mật khẩu; macOS/Linux đăng nhập tài khoản AD; secret trong OS credential store.
- WC ≥100k file: status < 3s sau lần quét đầu; checkout nhanh hơn svn CLI nhờ tải song song.
- Kill app giữa update/commit → work queue hoàn tất khi mở lại (như svn cleanup), không mất thay đổi local.

## Rủi ro chính
- **Semantics libsvn_wc phức tạp** (NODES op_depth, work queue, conflict skel, externals). Giảm thiểu: đọc code tham chiếu, test compat hai chiều mỗi PR, fuzz.
- **Hỏng dữ liệu**: journal qua `WORK_QUEUE` chuẩn, fsync, commit qua txn server (atomic), dogfood nội bộ dài.
- **Chạy song song với TortoiseSVN** cùng WC: tôn trọng `WC_LOCK` + khóa SQLite.
- Giao thức không có spec đầy đủ → code tham chiếu + bắt gói tin svn CLI ↔ VisualSVN.
- WebKitGTK trên Linux → spike phase 1, dự phòng Electron.
