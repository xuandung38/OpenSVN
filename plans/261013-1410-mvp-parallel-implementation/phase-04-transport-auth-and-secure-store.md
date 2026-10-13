---
phase: 4
title: "Transport auth and secure store"
status: pending
priority: P1
effort: "2–2.5 tuần · 1 agent"
dependencies: [1]   # cần gói 01a
wave: 1
owner_paths: ["crates/svn-ra/src/transport/**", "crates/svn-ra/src/auth/**", "crates/svn-ra/src/session.rs", "crates/secure-store/"]
---

# Phase 4: Transport auth and secure store

## Overview
Lớp HTTP thật cho engine: pool kết nối tới VisualSVN, TLS theo trust store OS, xác thực Basic + Negotiate (SSPI/GSSAPI), retry, cancel, prompt người dùng (credential/cert) qua callback; lưu secret vào OS credential store. P10 xây RA ops trên lớp này.

## Requirements
- Functional:
  - `HttpTransport::send(req: svn_protocol::HttpRequest) -> Response` với body stream (không buffer cả body) cả chiều gửi lẫn nhận; method tùy biến (PROPFIND, REPORT, MKCOL, MERGE, LOCK, UNLOCK, CHECKOUT…).
  - Pool tối đa N kết nối/host (mặc định 4, 1–8 như serf), keep-alive, **chỉ HTTP/1.1** (Negotiate/NTLM gắn theo kết nối); request body chunked, fallback Content-Length nếu proxy từ chối.
  - TLS: `rustls` + `rustls-platform-verifier`; lỗi verify → `AuthPrompter::confirm_certificate(CertInfo)` (contract 01a). **Chỉ chấp nhận `AcceptAlways` khi lỗi là `UnknownIssuer`/`SelfSigned`**; `HostnameMismatch`/`Revoked`/`Expired` → chặn cứng (không hỏi). Pin theo **(host, port) + SHA-256 leaf**; cert của host đã pin bị đổi → báo riêng "chứng chỉ đã thay đổi", không tự tin.
  - Auth: Basic (UTF-8) — **không bao giờ gửi Basic qua `http://`** (lỗi rõ); Negotiate: Windows dùng **SSPI package "Negotiate" trực tiếp** (crate `windows`, Kerberos→NTLM do SSPI tự chọn, giữ affinity kết nối trong handshake); macOS/Linux GSSAPI (`cross-krb5`/`libgssapi`, feature `negotiate`) — Basic là đường chính trên 2 OS này. Negotiate thất bại → **không tự hạ xuống Basic**: prompt hiển thị lý do, người dùng chọn.
  - **Credential gắn `OriginKey(scheme, host, port, realm)`**: chỉ tự gửi khi origin khớp chính xác; server khác cùng realm → phải prompt.
  - **Single-flight theo origin**: nhiều request cùng nhận 401 → 1 prompt, các request chờ; tối đa 3 lần sai rồi dừng (`AuthRequired`) để tránh khóa tài khoản AD. Sau khi thành công → gửi Basic preemptive cho origin đó.
  - **Gửi lại sau 401/reset**: body từ `ReplayableSource` (mở lại file) hoặc `Bytes`; body stream một lần → không retry, trả lỗi để caller (đã `ensure_authenticated` trước) xử lý.
  - Redirect chỉ theo **cùng origin**; khác origin → lỗi.
  - Retry tự động 3 lần (0.5/1/2s) chỉ cho method idempotent (GET, PROPFIND, OPTIONS, REPORT); timeout cấu hình; `CancelToken` hủy request đang chạy.
  - Proxy: hệ thống / thủ công.
  - `secure-store`: `get/set/delete(service="opensvn", key=OriginKey + user)` dùng **`keyring-core` + store theo OS** (Windows Credential Manager, macOS Keychain, Linux Secret Service qua store thuần Rust/zbus — chọn và pin trong 01a); Linux không có Secret Service → báo lỗi rõ, không lưu plaintext; mật khẩu proxy cũng lưu ở đây.
- Non-functional: redaction layer `tracing` theo denylist header (`Authorization`, `Proxy-Authorization`, `Cookie`, `Set-Cookie`, `Lock-Token`, `If`) + regex URL userinfo; mật khẩu `secrecy::SecretString`; throughput tải ≥ 90% `curl` cùng mạng.

## Architecture
```
crates/svn-ra/src/
  lib.rs                 pub mod transport, auth, session (ops do P10 thêm)
  transport/client.rs    HttpTransport (reqwest::Client cấu hình), pool, proxy, timeout
  transport/body.rs      adapter stream body (AsyncRead ↔ reqwest Body/bytes_stream)
  transport/retry.rs     chính sách retry + phân loại lỗi → svn_types::ErrorCode
  transport/tls.rs       rustls config + platform verifier + danh sách fingerprint đã tin
  auth/mod.rs            AuthState theo OriginKey, single-flight (AuthPrompter định nghĩa ở svn-ra-api)
  auth/basic.rs          header Basic
  auth/negotiate_sspi.rs (Windows)  auth/negotiate_gss.rs (macOS/Linux, feature "negotiate")
  session.rs             RaSessionCore: base URL, capabilities cache (từ OPTIONS — dùng svn-protocol), transport, auth
crates/secure-store/src/lib.rs   CredentialStore trait + impl keyring; MemoryStore cho test
```
App (P08) cài `AuthPrompter` bằng cách phát event `auth://prompt` và chờ `auth_respond`.

## Related Code Files
- Create: các file trên; `crates/svn-ra/tests/transport_*.rs` (wiremock), `crates/secure-store/tests/`.
- Modify: `Cargo.toml` workspace members/deps (chỉ thêm dòng).

## Implementation Steps
1. Client reqwest (HTTP/1.1) + rustls-platform-verifier; test HTTPS tới wiremock TLS với CA tự ký; test quy tắc AcceptAlways/chặn cứng/pin host:port/cert đổi.
2. Body streaming hai chiều; test upload/download 1 GB bộ nhớ phẳng.
3. Retry/timeout/cancel + phân loại lỗi; test bằng wiremock trả 503/timeout/reset.
4. Basic + 401 → prompter (single-flight) → gửi lại từ ReplayableSource; test: 3 lần sai thì dừng; 4 request song song → 1 prompt; Basic qua http bị chặn; 2 server cùng realm khác host → server 2 phải prompt; redirect khác origin bị chặn.
5. **Spike Negotiate tuần 1**: SSPI trực tiếp trên Windows join domain (FQDN và short name), GSSAPI macOS/Linux với `kinit`; ghi kết quả + quyết định NTLM; checklist VisualSVN IWA trong PR.
6. TLS untrusted → prompter; lưu/khớp fingerprint qua callback.
7. `secure-store` + test trên 3 OS (CI: Windows/macOS thật; Linux dùng `gnome-keyring` headless hoặc skip có lý do).
8. Proxy theo hệ thống (`reqwest` system proxy) + cấu hình thủ công.
9. Spike tài liệu: kết quả SSO trên Windows join domain + Kerberos macOS → ghi vào `docs/` qua PR cho lead.

## Success Criteria
- [ ] wiremock suite: Basic, 401 lặp, retry idempotent, không retry POST/PUT/MERGE, cancel giữa stream, TLS untrusted → prompter.
- [ ] Upload/download 1 GB: RAM đỉnh < 50 MB.
- [ ] Property test: secret ngẫu nhiên (mật khẩu, token, cookie, lock token, URL user:pass) không xuất hiện trong log ở mọi mức.
- [ ] Credential theo OriginKey: test 2 server cùng realm.
- [ ] Đăng nhập thành công VisualSVN thật: Windows SSO, macOS Basic, Linux Basic (ghi lại trong PR).
- [ ] `secure-store` set/get/delete chạy trên 3 OS.

## Risk Assessment
- Negotiate khác nhau giữa OS, short name không có SPN → spike tuần 1; Windows SSPI trực tiếp; fallback có prompt.
- `rustls-platform-verifier` trên Linux phụ thuộc bundle CA hệ thống → cho phép import CA thủ công (setting).
- reqwest không kiểm soát được affinity kết nối cho NTLM/method lạ → hạ xuống `hyper` trực tiếp (giữ API `HttpTransport`).
