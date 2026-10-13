# Phase 1 — Nền móng giao thức & test harness

## Mục tiêu
Nói chuyện được với VisualSVN bằng Rust thuần trên Windows/Linux/macOS; có test harness so sánh với svn CLI; kiểm chứng rủi ro UI Linux.

## Yêu cầu
- Cargo workspace + pnpm workspace, `rust-toolchain.toml`, clippy/rustfmt/cargo-deny, Biome.
- GitHub Actions matrix Windows/Ubuntu/macOS: build + test; job compat chạy Docker trên Ubuntu.
- `svn-protocol` (sans-IO):
  - `OPTIONS` + capabilities (`SVN-Me-Resource`, `SVN-Rev-Root-Stub`, `SVN-Txn-Stub`, `SVN-Youngest-Rev`, `SVN-Repository-UUID`, `SVN-Repository-Root`).
  - `PROPFIND` depth 0/1, `GET` `!svn/rvr/<rev>/<path>`.
  - XML streaming, không load cả body vào RAM.
- `svn-ra` transport: reqwest + rustls-platform-verifier, pool kết nối, retry, timeout, cancel.
- `svn-delta`: decode/encode svndiff0/1/2.
- Auth: Basic; Negotiate (SSPI Windows / GSSAPI macOS-Linux) mức spike; credential vào keyring.
- Tauri 2 skeleton: 1 command `repo_info(url)`.

## Spike (go/no-go)
1. **UI Linux**: list ảo 100k dòng + CodeMirror merge 20k dòng trên Ubuntu/Fedora (Intel/AMD/NVIDIA), Windows 11, macOS → đo fps/RAM. Fail → Electron (ADR-002).
2. **Auth AD**: Windows join domain SSO qua SSPI; macOS/Linux Basic + Kerberos `kinit`.
3. **TLS**: CA nội bộ được nhận từ trust store OS trên cả 3 OS.

## Test harness
- Docker: Apache httpd + mod_dav_svn + svn CLI 1.14 và 1.15.
- Fixture repo: text, binary lớn, props, `svn:needs-lock`, rename, externals, nhiều branch, đường dẫn Unicode/khoảng trắng.
- Compat runner: chạy cùng thao tác bằng engine và svn CLI, so sánh kết quả.
- cargo-fuzz cho svndiff decoder và XML parser.

## Tiêu chí xong
- `repo_info`, list thư mục, tải file đúng byte so với `svn cat` trên cả 3 OS.
- svndiff roundtrip + fuzz 1h không crash.
- 3 spike có kết luận.
