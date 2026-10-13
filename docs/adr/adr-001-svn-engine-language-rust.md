# ADR-001: Ngôn ngữ viết engine SVN — Rust

**Status:** Accepted
**Date:** 2026-10-13
**Deciders:** Tech lead, PO

## Context
- Tự viết engine SVN (không dùng svn CLI/libsvn) để tối ưu & mở rộng lâu dài.
- Phạm vi hiện tại: Windows, Linux, macOS. iPadOS/Android để sau → engine phải nhúng được dạng thư viện (iOS cấm spawn process), không chặn đường mobile.
- Repo rất lớn, binary lớn → cần I/O song song, kiểm soát bộ nhớ, không GC pause khi xử lý stream GB.
- Lỗi engine = hỏng working copy/dữ liệu → cần an toàn bộ nhớ, type system chặt.
- Không có crate Rust nào cài giao thức HTTP/WebDAV của SVN: `svn` crate chỉ có `svn://`, `subversion` crate là binding C.

## Decision
Viết engine bằng **Rust (edition 2024, stable)**, ưu tiên dependency **pure Rust** để build Windows/Linux/macOS (và mobile sau này) không cần C toolchain phức tạp (ngoại lệ có kiểm soát: SQLite qua `rusqlite` bundled).

## Options Considered

### Option A: Rust
| Dimension | Assessment |
|---|---|
| Complexity | Med–High (learning curve) |
| Cost | Không phí license |
| Scalability | Async I/O (tokio), zero-cost, không GC |
| Team familiarity | Cao (PO xác nhận team mạnh Rust + React) |

**Pros:** 1 codebase cho Windows/Linux/macOS, build static lib cho iOS/Android khi cần; an toàn bộ nhớ; hệ sinh thái mạnh (hyper, rustls, quick-xml, rusqlite); có tiền lệ VCS lớn viết bằng Rust (gitoxide, Jujutsu); nhúng trực tiếp vào Tauri.
**Cons:** compile chậm; tuyển người khó hơn; async Rust phức tạp.

### Option B: Go
| Dimension | Assessment |
|---|---|
| Complexity | Low–Med |
| Cost | Không |
| Scalability | Tốt, có GC |
| Team familiarity | Thường cao hơn Rust |

**Pros:** dễ viết, net/http tốt. **Cons:** gomobile cho iOS/Android kém linh hoạt, binary lớn, GC; giao tiếp với UI qua cgo cồng kềnh; không ghép tự nhiên với Tauri/Flutter.

### Option C: Kotlin Multiplatform
**Pros:** chia sẻ Android/iOS/JVM desktop. **Cons:** Kotlin/Native I/O và thư viện HTTP/TLS/SQLite cấp thấp yếu hơn; desktop chạy JVM nặng; Linux desktop chưa là ưu tiên của hệ sinh thái.

### Option D: C++
**Pros:** hiệu năng, chạy mọi nơi. **Cons:** an toàn bộ nhớ kém (rủi ro hỏng dữ liệu), build/dependency đa nền tảng tốn công, năng suất thấp.

## Trade-off Analysis
Yếu tố quyết định là **an toàn dữ liệu + hiệu năng I/O + giữ đường nhúng lên mobile**. Rust là lựa chọn duy nhất đạt cả ba mà vẫn có hệ sinh thái mạng/XML/DB trưởng thành. Chi phí là learning curve — chấp nhận được vì engine là tài sản lõi lâu dài.

## Consequences
- Dễ hơn: 1 codebase engine cho mọi nền tảng; fuzz/property test; tối ưu sâu.
- Khó hơn: thời gian compile CI (dùng sccache/rust-cache).
- Xem lại: khi bắt đầu làm mobile — kiểm tra lại toàn bộ dependency build được iOS/Android.

## Action Items
1. [ ] Thiết lập workspace Cargo, `rust-toolchain.toml` cố định phiên bản, clippy + rustfmt trong CI.
2. [ ] Chính sách dependency: pure Rust mặc định; mọi crate có C phải được duyệt (ghi vào ADR-003).
