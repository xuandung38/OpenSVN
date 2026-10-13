# ADR-003: Bộ thư viện cho engine Rust

**Status:** Accepted
**Date:** 2026-10-13
**Deciders:** Tech lead

## Context
- Engine build cho Windows, Linux, macOS (mobile sau) → ưu tiên pure Rust (ADR-001).
- Máy Windows thường join domain AD → đăng nhập một lần (SSO) là lợi thế lớn.
- Giao thức SVN qua HTTP(S): method tùy biến (PROPFIND, REPORT, MKCOL, LOCK, UNLOCK, MERGE), body XML lớn dạng stream, svndiff base64 trong XML.
- Server VisualSVN dùng CA nội bộ; đăng nhập tài khoản AD.
- Working copy ≥100k file; file binary lớn.

## Decision

### Kiến trúc engine: "sans-IO" core
Lớp giao thức (dựng request, parse response, delta editor) **không làm I/O**, nhận/trả byte stream → test bằng fixture, fuzz dễ; lớp transport và FS tách riêng.

### Bảng lựa chọn
| Nhu cầu | Chọn | Lý do / phương án khác |
|---|---|---|
| Async runtime | `tokio` | Chuẩn de facto; hyper/reqwest chạy trên nó |
| HTTP client | `reqwest` (hyper 1.x), HTTP/1.1 + pool nhiều kết nối, h2 nếu server ALPN | Method tùy biến qua `Method::from_bytes`, body stream. Dùng thẳng `hyper` nếu cần kiểm soát sâu hơn |
| TLS | `rustls` + `rustls-platform-verifier` | Dùng trust store OS (Windows CryptoAPI, macOS Keychain, Linux `/etc/ssl`) → CA nội bộ đẩy qua GPO/MDM tự hoạt động; không cần OpenSSL |
| Auth | Basic (tài khoản AD, chỉ qua HTTPS) + **Negotiate/SPNEGO**: Windows gọi **SSPI package "Negotiate" trực tiếp** (crate `windows`; SSPI tự chọn Kerberos/NTLM, giữ affinity kết nối); macOS/Linux GSSAPI (`cross-krb5`/`libgssapi`, feature `negotiate`) | Windows join domain: SSO. Không tự hạ Negotiate → Basic (phải prompt). Chỉ HTTP/1.1 |
| XML | `quick-xml` (reader streaming) | Nhanh, không cấp phát cây DOM; tránh serde cho response lớn |
| Base64 | `base64` | svndiff nhúng trong update-report |
| Nén | `flate2` (backend `miniz_oxide`), `lz4_flex` | Cả hai pure Rust; svndiff1=zlib, svndiff2=lz4. Content store dùng lz4 (bỏ qua file đã nén) |
| Checksum | `md-5`, `sha1` (RustCrypto) | SVN dùng MD5 (text-delta base) + SHA-1 (fulltext); key content store = SHA-1 như pristine svn |
| DB working copy & cache | `rusqlite` (feature `bundled`); **wc.db giữ nguyên journal mode/busy setting như svn** (không bật WAL); WAL chỉ cho `app.sqlite`/`cache.sqlite` | **Bắt buộc**: `.svn/wc.db` là SQLite (ADR-004) |
| Duyệt cây file | `ignore` (WalkParallel) + `globset` | Quét song song 100k file; tự cài luật `svn:ignore`/`global-ignores` |
| FS watcher (desktop) | `notify` | FSEvents/inotify |
| Ghi file an toàn | `tempfile` + rename atomic + fsync | Chống hỏng WC khi crash |
| Diff | `imara-diff` (histogram/Myers) | Nhanh, dùng bởi gitoxide/helix; diff3 tự viết theo semantics `libsvn_diff` (`diffy` làm tham chiếu) |
| Secret | `keyring-core` + store theo OS (Windows Credential Manager, macOS Keychain, Linux Secret Service qua store thuần Rust/zbus); khóa theo origin (scheme, host, port, realm) | keyring 4.x đã tách thành keyring-core + crate store; Android cần store riêng khi làm mobile |
| Path Windows | tự chuẩn hóa: long path `\\?\`, không phân biệt hoa/thường, tên cấm (CON, NUL...), retry rename khi antivirus giữ file | Hành vi giống libsvn (`svn_io_file_rename2` retry) |
| Lỗi | `thiserror` (lib), `anyhow` (app) | |
| Log/trace | `tracing` + `tracing-subscriber` | Ẩn mật khẩu/token trong log |
| Serialize sang UI | `serde` + `specta`/`tauri-specta` | Sinh type TS |
| Test | `cargo-nextest`, `proptest`, `insta` (snapshot), `cargo-fuzz`, `wiremock` | + differential test với svn CLI 1.14 trong Docker (Apache + mod_dav_svn) |

## Options Considered (điểm chính)
- **OpenSSL/native-tls vs rustls**: native-tls cần OpenSSL trên Linux; rustls + platform verifier đồng nhất mọi OS và vẫn tôn trọng CA doanh nghiệp.
- **SQLite vs redb/sled**: SQLite được kiểm chứng hàng chục năm về chống hỏng dữ liệu, có công cụ debug; chấp nhận 1 dependency C.
- **imara-diff vs similar**: imara-diff nhanh hơn rõ trên file lớn; `similar` dễ dùng hơn nhưng chậm.

## Consequences
- Dễ hơn: build 3 desktop OS chỉ cần `cargo`, không cần OpenSSL/APR/serf; mobile sau chỉ thêm target.
- Khó hơn: tự cài Basic/Negotiate, diff3, svndiff — chấp nhận vì là lõi sản phẩm.
- **Ngoại lệ thư viện C được phép**: SQLite (`rusqlite` bundled), GSSAPI hệ thống (macOS/Linux, feature `negotiate`), WebKitGTK (runtime Tauri trên Linux), `ring` (crypto provider của rustls: C/asm tự chứa biên dịch bằng `cc`, không cần thư viện hệ thống; chỉ chuyển sang `aws-lc-rs` nếu sau này cần FIPS hoặc trao đổi khóa hậu lượng tử). Thêm ngoại lệ mới phải cập nhật ADR này.
- Xem lại: nếu CA nội bộ không có trong trust store OS (Linux thường thiếu) → cho phép import CA trong app.

## Action Items
1. [ ] Spike TLS + auth: kết nối VisualSVN nội bộ từ Windows (SSPI SSO), macOS, Linux (Basic + Kerberos kinit).
2. [ ] Spike HTTP: PROPFIND/REPORT bằng reqwest, stream 1GB không tăng bộ nhớ.
3. [ ] `deny.toml` (cargo-deny): chặn license không phù hợp, cảnh báo crate có C.
