# Review PR #31 — P04 transport/auth M1

- PR: xuandung38/OpenSVN#31, head `fa839b443fb9a1aa28cd7922cc224f5bee788dfc` (kiểm tra lại cuối review, không đổi)
- Scratch: worktree `<local>/OpenSVN-worktrees/review-pr31`, gates chạy trên bản rsync trong scratchpad (không có target/)
- Phạm vi: 25 file, +5533/−33 (≈1.35k là Cargo.lock)

## Verdict: REQUEST CHANGES

Lõi bảo mật về cơ bản đúng: redirect tắt trong reqwest và tự xử lý theo same-origin, Basic chỉ qua https, userinfo trong URL bị từ chối, pin không đè được lỗi cứng, cert self-signed sai host bị khóa cứng, redaction có property test thật. Không có lỗi CRITICAL. Phải sửa 4 điểm IMPORTANT trước khi merge (I1–I4); các điểm MODERATE có thể làm follow-up.

## Gates (đã chạy trên head fa839b4, macOS arm64)

| Gate | Kết quả |
|---|---|
| `cargo fmt --all --check` | sạch |
| `cargo clippy --workspace --all-targets --all-features --locked -D warnings` | sạch |
| `cargo test --workspace --locked` | xanh hết (svn-ra: TLS 9, auth 13, http 15, redaction 4, session 1; secure-store 4 + 1 ignored) |
| `cargo deny check` | advisories/bans/licenses/sources ok |
| unwrap/expect/unsafe trong lib | không có (chỉ có trong `#[cfg(test)]`) |
| openssl / openssl-sys / native-tls / aws-lc-sys | không có trong Cargo.lock |
| `h2` | có trong lock; `cargo tree -e normal,build -i h2 --target all` trả về rỗng → chỉ ở test (dev graph). Không ship |
| `libdbus-sys` | có, nhưng do `tauri → tao → dbus` (có từ trước, không phải P04). Câu "không có libdbus-sys, h2" trong mô tả PR không chính xác |

## Checklist bảo mật (10 mục lead yêu cầu)

1. **Credential theo origin / cross-origin / http / Negotiate→Basic**: ĐẠT. `auth/mod.rs:33-62` key = (scheme, host, port) + realm; `prepare` chỉ gắn Basic khi `https` (`auth/mod.rs:132-135`); `on_unauthorized` từ chối http (`:172-177`); redirect `Policy::none()` (`client.rs:135`) và `Authorization` của caller bị xóa (`client.rs:83`); Negotiate+Basic được đưa cho prompter chọn, method khác Basic → lỗi (`auth/mod.rs:233-238`). Không có hạ cấp ngầm.
2. **TLS**: ĐẠT về chính sách, có 1 lỗi cách phân loại trên macOS (I2). Chỉ UnknownIssuer/SelfSigned được override (`tls.rs:346-349`); kiểm tra hạn của leaf chạy trước (`tls.rs:273-280`); override sai host → HostnameMismatch (`tls.rs:339-345`); pin chỉ được xét trên nhánh override được (`tls.rs:351-354`), nên không bao giờ đè lỗi cứng. Không có config nào bỏ qua được verifier (`extra_root_certs` chỉ thêm root, đúng ý đồ import CA). Cert đã pin mà bị đổi → hỏi lại kèm thông báo riêng, `AcceptAlways` không pin (`client.rs:239-247`). Phase 04 ghi "báo riêng, không tự tin" (phase-04 dòng 18), nên **pin mismatch không phải lỗi cứng theo spec**. Nếu lead muốn chặn cứng đúng như brief thì phải đổi spec trước.
3. **Redirect same-origin, userinfo**: ĐẠT (`client.rs:388-417`, `validate_url` `client.rs:365-381`). Chuyển https→http cùng host bị chặn vì khác scheme. Kiểm tra same-origin cho href nằm ở P03/P10, không thuộc PR này.
4. **Redaction**: ĐẠT với denylist header trong spec (`redact.rs:17-24`) và userinfo. Property test thật (6 kiểu log, chạy qua cả writer). Còn hở với token nằm ngoài header (M1).
5. **Streaming**: ĐẠT. Upload qua `ReaderStream` 64 KiB, download theo `chunk()`; `bytes(limit)` có giới hạn; có connect/read timeout; cancel đã race ở permit, send, backoff, chunk. Riêng prompt credential thì không cancel được (I1).
6. **secure-store**: ĐẠT. Store theo từng OS (Keychain `keychain`, Credential Manager, zbus `crypto-rust`), không có fallback plaintext (`lib.rs:203-213`), `map_error` dùng `Display` của keyring-core và Display này không in bytes secret (đã đọc `keyring-core-1.0.1/src/error.rs:86-97`). `svn_ra_api::Credentials` derive `Debug` nhưng `password: SecretString` in ra `[REDACTED]`, nên không lộ. Zeroize chưa thật sự làm (M2).
7. **Dependency**: không có openssl/native-tls/aws-lc. `ring` là ngoại lệ C/asm chưa có trong ADR-003 (I3).
8. **Ownership**: thay đổi ngoài path sở hữu gồm root `Cargo.toml` (+13 dòng, chỉ thêm) và `crates/svn-ra/Cargo.toml` (01a sở hữu, chỉ thêm dep). Cả hai tối thiểu và PR có ghi rõ, nhưng plan.md quy định "dep mới = PR `contract:` riêng" (M7).
9. **Gates**: xanh hết (bảng trên).
10. **`reason` cho AuthPrompter**: đồng ý, xem phần cuối.

## IMPORTANT

### I1. Prompt credential và các request đang chờ gate không cancel được
- `auth/mod.rs:186` `flight.gate.lock().await` và `auth/mod.rs:226` `self.prompter.credentials(..).await`: cả hai đều không race với `CancelToken`, và `on_unauthorized` không nhận token (`client.rs:192-194`).
- Hậu quả: người dùng bấm Cancel thao tác (doc 02 yêu cầu hủy < 1s) trong khi dialog đăng nhập đang mở hoặc UI không trả lời, thì request treo vô hạn. N request khác của cùng origin xếp hàng trên gate cũng treo theo. Prompt cert thì đã làm đúng (`client.rs:233-237`), nhưng `prompt_gate.lock()` (`client.rs:225`) cũng chưa race với cancel.
- Sửa: truyền `cancel` vào `on_unauthorized`; bọc cả `gate.lock()` lẫn `prompter.credentials()` trong `tokio::select! { biased; () = cancel.cancelled() => return Err(cancel_err), r = ... }`. Nếu future prompt bị drop do cancel thì không tăng generation và không set `denied`, để request khác vẫn còn được hỏi lại. Làm tương tự với `prompt_gate.lock()`. Thêm test: prompter chặn mãi mãi, cancel → `ErrorCode::Cancelled` trong vòng 100 ms.

### I2. macOS: phân loại "untrusted anchor" dựa trên chuỗi tiếng Anh
- `tls.rs:302`: `CertificateError::Other(o) if o.to_string().contains("not trusted")`. Chuỗi này đến từ `rustls-platform-verifier-0.7.1/src/verification/apple.rs`, nhánh fallback `invalid_certificate(format!("{trust_error}: {err_code}"))`, tức là mô tả CFError của Security.framework.
- Rủi ro 1 (chức năng): mô tả CFError của Security.framework nhiều khả năng đã được bản địa hóa theo ngôn ngữ hệ thống. Trên macOS đặt tiếng Việt/Nhật thì `"not trusted"` không khớp → `Other` → khóa cứng, và cert self-signed của VisualSVN **không bao giờ được hỏi**. Đây đúng là kịch bản chính của MVP. Chưa kiểm chứng trên macOS đã bản địa hóa.
- Rủi ro 2 (bảo mật): khớp chuỗi con có thể bắt nhầm các mã lỗi khác có chữ "not trusted" (ví dụ cert mà người dùng/MDM đặt "Never Trust"), khiến chúng thành override được.
- Sửa: phân loại theo mã OSStatus ở đuôi chuỗi (`: -67843` = `errSecNotTrusted`; thêm `-67845`/`errSecCreateChainFailed` nếu cần), không dựa vào chữ. Tốt hơn nữa là parse `-?\d+$` rồi khớp danh sách mã cho phép. Thêm unit test cho `classify` với chuỗi `"«x» n'est pas fiable: -67843"` → UnknownIssuer, và với một mã khác → Other.

### I3. `ring` vi phạm DoD ADR-003 nếu chưa cập nhật ADR
- `crates/svn-ra/Cargo.toml:19-20` (`rustls` feature `ring`); `ring v0.17.14` mới xuất hiện trong lock. plan.md DoD: "Khác → cập nhật ADR-003 **trước**". ADR-003 dòng 50 chỉ liệt kê SQLite/GSSAPI/WebKitGTK.
- **Khuyến nghị cho lead: giữ `ring`**, thêm `ring` (crypto provider của rustls: C + asm tự chứa, chỉ cần `cc`, không cần thư viện hệ thống) vào danh sách ngoại lệ ADR-003. Lý do:
  - Phần C/asm nhỏ hơn nhiều so với AWS-LC; đã là provider mặc định của rustls nhiều năm; build được trên mọi target, kể cả mobile sau này.
  - aws-lc-rs chỉ đáng chọn khi cần FIPS hoặc trao đổi khóa hậu lượng tử `X25519MLKEM768`, mà provider ring của rustls 0.23 không có. Cả hai không phải yêu cầu của MVP và có thể xem lại sau.
  - Không có lựa chọn pure-Rust đủ chín (rustls-rustcrypto chưa được audit).
- Phụ: comment "no cmake/NASM needed on Windows, unlike aws-lc-rs" (`svn-ra/Cargo.toml:19`) có thể đã lỗi thời, vì aws-lc-sys bản gần đây có NASM dựng sẵn và không cần cmake với bản non-FIPS. Nên kiểm chứng rồi nói lý do thật (kích thước/bề mặt C) thay vì lý do build.
- Ai làm: lead sửa ADR (docs/ do lead sở hữu). PR không cần đổi code.

### I4. Vòng auth không có trần → server lỗi/độc hại có thể làm client lặp vô hạn
- `client.rs:178-213`: vòng lặp chỉ dừng khi `on_unauthorized` lỗi. Nếu server trả 401 xen kẽ realm A/B cho cùng URL, mỗi flight đều có credential nên đi nhánh `resolved_elsewhere` (`auth/mod.rs:194-203`) và không bao giờ tăng `failures` → lặp mãi (chỉ thoát được khi cancel).
- Sửa: đếm số vòng auth trong `send_authenticated` (ví dụ `MAX_ATTEMPTS * 2`), vượt thì trả `AuthRequired("server kept rejecting credentials")`. Thêm test với handler xen kẽ realm.

## MODERATE

- **M1. Redaction chỉ theo header.** Đã kiểm chứng bằng probe: `redact_text("<D:href>opaquelocktoken:abcd-1234</D:href>")` và `redact_text("password=hunter2")` giữ nguyên secret. P10 (LOCK/UNLOCK, body có lock token) và P08 (log lỗi) dễ log nhầm. Sửa: thêm quy tắc `opaquelocktoken:[^\s<>)"']+` → `opaquelocktoken:***` và key `password|passwd|secret|token` vào `redact_text` (`redact.rs:161`), mở rộng proptest. Lưu ý: hiện chưa có chỗ nào cài `RedactingMakeWriter` (grep apps/ và crates/ không thấy `tracing_subscriber`), nên P08 bắt buộc phải cài, cần ghi vào handoff.
- **M2. "Scrub" trong `basic_header` không xóa gì cả.** `auth/basic.rs:11-14`: `plain.clear()` chỉ đặt len = 0, bytes vẫn còn trong heap; comment "Best-effort scrub" gây hiểu nhầm. Sửa: `use secrecy::zeroize::Zeroize; plain.zeroize();`, áp dụng cho cả `encoded` và chuỗi `format!("Basic {encoded}")`. `HeaderValue` (chứa base64 ≡ mật khẩu) không zeroize được, nên ghi rõ giới hạn này. Tương tự, `secure-store/src/lib.rs:148` `SecretString::from(String)` có thể realloc khi shrink và để lại bản sao; chấp nhận được nhưng nên ghi chú.
- **M3. Không xoay được pin.** `TrustStore` chỉ có `pinned`/`pin` (`tls.rs:27-32`), và cert đã đổi thì không pin được (`client.rs:244-247`). Khi cert self-signed hợp lệ được gia hạn, người dùng sẽ bị hỏi lại mỗi session mãi mãi. Hơn nữa UI chỉ biết "đã đổi" qua chuỗi `CertFailure::Other("the certificate changed…")` (`client.rs:228-232`), không có kiểu riêng. Đề xuất: thêm `CertFailure::Changed` (hoặc `CertInfo.pin_changed: bool`) vào contract, và `TrustStore::replace_pins(host, port, fp)` cho trường hợp người dùng chủ động thay. Giữ mặc định là không tự tin.
- **M4. Slot `problem` dùng chung theo origin có thể cũ hoặc nhầm kết nối.** `tls.rs:112, 186-188`, `client.rs:329-333`. Lỗi cứng (ví dụ hostname mismatch) không bao giờ được xóa trừ khi có kết nối thành công, nên một lỗi mạng thuần (`is_connect`/`is_request`) về sau bị báo thành lỗi TLS cũ. Handshake song song cũng ghi đè lẫn nhau. Không bypass được, vì verify vẫn xét theo fp ở từng handshake, nhưng thông báo sai. Sửa: `take()` problem khi đọc (`problem.lock().take()`) và/hoặc gắn với một attempt id.
- **M5. Permit pool bị giữ đến khi drop response** (`request.rs:145`, `client.rs:325`). Nếu caller giữ ≥ N response đang stream của cùng origin rồi gửi thêm request thì treo đến khi cancel. Đây là rủi ro thiết kế cho P10 update (REPORT stream + GET song song). Sửa: ghi vào doc `HttpTransport::send` và handoff P10, hoặc dùng `try_acquire` với timeout để trả lỗi rõ ràng.
- **M6. Lệch so với spec/docs (lead quyết):**
  - Doc 02 dòng 115 ghi key `opensvn:<scheme>://<host>:<port>/<realm>:<user>`, còn code dùng service `opensvn` + account `scheme|host|port|realm|user` có escape (`secure-store/src/lib.rs:61-81`). Cách của code là injective nên tốt hơn → cập nhật doc 02.
  - Phase 04 dự kiến `send(req: svn_protocol::HttpRequest)`, còn PR tự định nghĩa `HttpRequest/HttpResponse` trong `svn-ra::transport`. Cần chốt với P03 để không có 2 kiểu song song.
  - "Fallback Content-Length nếu proxy từ chối chunked" chưa làm (`RequestBody::Stream{len: None}` luôn đi chunked).
  - Các item đã ghi là M2 trong PR (Negotiate, 1 GiB CI, throughput so với curl, login VisualSVN thật) thì không tính là lỗi.
- **M7. Quy trình contract.** Dep mới trong root `Cargo.toml` và `crates/svn-ra/Cargo.toml` (01a sở hữu) cần PR `contract:` riêng theo plan.md quy tắc 2. Nội dung tối thiểu, chỉ thêm dòng, PR đã ghi rõ. Khuyến nghị: lead duyệt inline ngay trong PR này (tách ra chỉ tạo churn), nhưng ghi lại quyết định. `tracing-subscriber` trở thành dep normal của engine lib chỉ để có `MakeWriter`; cân nhắc đưa sau feature `log-redaction` hoặc chuyển writer sang app. Ưu tiên thấp.
- **M8. Windows: thiếu intermediate (`CERT_E_CHAINING`) → `Other` → khóa cứng** (mapping ở `rustls-platform-verifier-0.7.1/src/verification/windows.rs:681-687`). Lỗi theo hướng an toàn nhưng chặn trường hợp CA nội bộ thiếu chain mà svn/TortoiseSVN cho override. Ghi vào test thủ công trên Windows của M2.
- **M9. Lặt vặt về redirect.** `client.rs:98-104`: với 303, body sẽ bị bỏ, nhưng body không replay được vẫn bị kiểm tra và báo lỗi trước. Ngoài ra transport tự theo 301/302/307/308 cho cả PUT/MERGE/MKCOL; svn thường coi 301 trên repo là "repository moved" và báo người dùng. Cần xác nhận ý đồ với P10.

## Ghi chú (không chặn merge)
- Proxy `Manual` gửi Basic proxy-auth qua proxy `http://` (`client.rs:152-154`). Đây là thực hành chuẩn, và quy tắc "không Basic qua http" trong spec nói về server. Cần lead xác nhận là ổn.
- Preemptive Basic gửi tới mọi path của cùng (scheme, host, port) theo realm thành công gần nhất (`auth/mod.rs:127-135`). Chấp nhận được vì realm chỉ biết được sau challenge và vẫn là cùng một server.
- Verifier gọi `TrustStore::pinned()` đồng bộ ngay trong handshake (`tls.rs:146, 355-359`), nên impl của P08 không được truy cập đĩa hay khóa lâu (doc comment đã ghi).

## Ý kiến về đổi contract: thêm `reason` vào `AuthPrompter`
Đồng ý, và nên làm ngay (PR `contract:`) trước khi P08 implement prompter, để không phải sửa thêm một lần nữa. Phase 04 yêu cầu "Negotiate thất bại → prompt hiển thị lý do". Đề xuất gom tham số vào một struct `#[non_exhaustive]` để mở rộng sau mà không phá API:
```rust
#[non_exhaustive]
pub struct CredentialRequest<'a> { pub origin: &'a OriginKey, pub attempt: u8, pub offered: &'a [AuthMethod], pub reason: PromptReason }
#[non_exhaustive]
pub enum PromptReason { Challenge, Rejected, NegotiateFailed { detail: String } }
async fn credentials(&self, req: &CredentialRequest<'_>) -> Option<Credentials>;
```
Đi kèm là `CertFailure::Changed` (M3), cũng trong `svn-ra-api`. `detail` không được chứa token SPNEGO.

## Việc cần làm (ưu tiên)
1. I1: cho prompt credential và các gate cancel được, kèm test.
2. I2: phân loại macOS theo mã OSStatus, không theo chuỗi, kèm unit test `classify`.
3. I4: đặt trần số vòng auth, kèm test realm xen kẽ.
4. I3 (lead): thêm `ring` vào ngoại lệ ADR-003.
5. M1, M2: mở rộng redaction cho lock token/password trong văn bản; zeroize thật hoặc sửa comment.
6. Contract PR: `reason` + `CertFailure::Changed` (+ `replace_pins` nếu đồng ý M3).
7. Docs (lead): format key ở doc 02; chốt kiểu `HttpRequest` với P03.

## Câu hỏi chưa giải quyết
- Lead có muốn pin mismatch là **lỗi cứng** (như brief) thay vì "hỏi lại, không tự pin" (như phase 04)? Nếu có thì phải đổi spec trước.
- Proxy Basic qua proxy `http://` có chấp nhận không?
- Có cho transport tự theo redirect với method ghi (PUT/MERGE/MKCOL) không, hay trả 301 lên P10?
- Chưa kiểm chứng: chuỗi CFError trên macOS đã bản địa hóa (I2), và đường Linux zbus/Windows store (CI sẽ chạy).

Status: DONE
Summary: Review PR #31 xong. Gates xanh (fmt, clippy, test, deny); lõi chính sách auth/TLS/redirect/redaction đúng spec, không có CRITICAL. Verdict REQUEST CHANGES vì 4 IMPORTANT: prompt credential không cancel được, phân loại macOS dựa trên chuỗi tiếng Anh, `ring` chưa có trong ADR-003, vòng auth không có trần.

---

## Re-review — head `a200f1a` (2026-10-13)

Commit đã xét: `5189821` (fix), `0823755` (docs secure-store), `a200f1a` (merge main + adapt #43). Head chưa có `origin/main` mới nhất (`a3fd709`). `git merge-tree` cho thấy merge sạch, GitHub báo `MERGEABLE`.

### Gates (local, CARGO_BUILD_JOBS=2; CI bị skip)
- `cargo fmt --all --check`: sạch.
- `cargo clippy -p svn-ra -p secure-store -p svn-ra-api --all-targets --all-features --locked -D warnings`: sạch.
- `cargo test -p svn-ra -p secure-store -p svn-ra-api --all-features --locked`: xanh hết (auth 16, http 18, tls 10 + 2 unit test `classify`, ...).
- `cargo deny check`: ok.
- Không có unwrap/expect/unsafe trong lib.
- Giới hạn: lần chạy `--workspace` đầu tiên hỏng giữa chừng vì scratch copy bị xóa trong lúc dọn đĩa khẩn cấp. Lần chạy lại chỉ nhắm vào 3 crate trên. Không crate nào khác phụ thuộc `svn-ra`/`secure-store` (đã grep các Cargo.toml). Thay đổi `svn-ra-api` đến từ main (#43) và đã được gate ở đó.
- Đã `cargo clean` (xóa 1.6 GiB) và xóa scratch copy.

### (a) Các finding cũ
| # | Trạng thái | Bằng chứng |
|---|---|---|
| I1 prompt/gate cancel được | **FIXED** | `auth/mod.rs` `or_cancel` bọc `gate.lock()` và `prompter.credentials()`; prompt bị hủy thì không đụng tới flight. `client.rs` cert `prompt_gate` cũng bọc `or_cancel`. Test `cancel_interrupts_a_prompt_that_never_answers` và `cancel_interrupts_a_request_queued_behind_a_prompt` (< 1s, vẫn chỉ 1 prompt) |
| I2 macOS theo OSStatus | **FIXED** | `tls.rs` `is_untrusted_anchor`: parse mã ở đuôi chuỗi, chỉ chấp nhận `-67843`. Unit test với chuỗi tiếng Pháp/Nhật, và mã khác/chỉ có chữ → `Other` (hard) |
| I3 ring trong ADR-003 | **FIXED** | ADR-003 Consequences có dòng ngoại lệ `ring`, nêu điều kiện chuyển sang aws-lc-rs (FIPS/PQ). Comment ở `svn-ra/Cargo.toml` đã sửa |
| I4 vòng auth có trần | **FIXED** | `client.rs` `MAX_AUTH_ROUND_TRIPS=10`, `MAX_CHALLENGES_PER_REALM=4`. Test realm xen kẽ: ≤ 10 request, `AuthRequired` |
| M2 zeroize | **FIXED** | `basic.rs` zeroize `plain`/`encoded`/`text`; giới hạn của `HeaderValue` được ghi trong comment |
| M4 cache lỗi cert cũ | **FIXED** | `problem_since(started)`: chỉ nhận problem ghi từ attempt hiện tại trở đi |
| M5 slot | **FIXED (một phần, chấp nhận được)** | Permit được nhả ngay khi body kết thúc (`request.rs` `chunk()`). Test: REPORT 10 GiB đang stream + 8 GET song song với pool = 2 không deadlock. Caller giữ N stream chưa đọc xong thì vẫn chặn; đây là bản chất của pool có giới hạn |
| M9 redirect | **FIXED** | `follows_redirects`: chỉ GET/PROPFIND/OPTIONS/REPORT; method ghi trả lỗi "repository moved". Test `write_methods_are_never_redirected`. Nhỏ: `HEAD` không có trong danh sách (an toàn, chỉ báo lỗi thay vì theo) |
| Proxy Basic qua proxy http | **DOCUMENTED** | Doc của `ProxyConfig::Manual`: chỉ gửi khi người dùng tự cấu hình; 407 từ proxy không được cấu hình không bao giờ prompt. Chấp nhận |
| M6 key format doc 02 | **FIXED** | doc 02 đã đổi sang `scheme\|host\|port\|realm\|user` có escape |
| M1 redaction token ngoài header | **OPEN** (không chặn) | `redact.rs` không đổi; `opaquelocktoken:` trong XML và `password=` vẫn lọt. Cần làm trước khi P10 LOCK/UNLOCK log body và trước khi P08 cài writer |
| M8 Windows `CERT_E_CHAINING` | OPEN (M2, test thủ công Windows) | không đổi |
| M6 kiểu `HttpRequest` vs P03, fallback chunked | OPEN (điều phối/M2) | không đổi |

### (b) Adapt #43
- `CredentialRequest`/`PromptReason`: transport dựng request bằng `CredentialRequest::new`. `reason = Initial` khi `attempt == 0`, ngược lại là `Rejected`. Test `wrong_then_right_credentials_reports_attempt_numbers` kiểm tra `[Initial, Rejected]`. `NegotiateFailed` chưa dùng (M2). Ghi chú: sau khi bị từ chối 3 lần (failures reset về 0), lần hỏi kế tiếp lại là `Initial`; chấp nhận được.
- `CertFailure::Changed{old_sha256,new_sha256}`: chỉ được gán khi problem override được (`client.rs` resolve_certificate). `old` = pin cuối cùng (`previous.last()`); nếu host có nhiều pin thì UI chỉ thấy một. Chấp nhận. Test so khớp chính xác cả `old` lẫn `new`.
- Thứ tự ưu tiên: đúng với doc comment của contract. Kiểm tra ngày của leaf chạy trước; hostname mismatch/expired/revoked/not-yet-valid không override được, nên `previous` để rỗng và `changed=false`. `Changed` chỉ thay cho SelfSigned/UnknownIssuer. Nhánh `_ =>` cho biến thể `#[non_exhaustive]` lạ là hard reject.
- Test `changed_and_hostname_mismatched_is_reported_as_hostname_mismatch`: pin của `localhost` khác cert, cert lại cấp cho `other.test.local` → `TlsUntrusted`, thông báo có "host name" và không có "changed", 0 prompt, pin giữ nguyên. **Đạt**, và kiểm tra đúng ý đồ.
- Known gap (đã chấp nhận, không chặn): `AcceptAlways` trên `Changed` → `PinAction::Replace` (`client.rs`), mâu thuẫn với doc của `CertDecision::AcceptAlways` trong `svn-ra-api` ("treated as AcceptOnce unless the app explicitly replaces the pin"). Follow-up `CertDecision::ReplacePin` phải đồng thời: (1) với `Changed`, chỉ nhận `Reject`/`ReplacePin`; `AcceptAlways`/`AcceptOnce` → coi như Reject hoặc chỉ cho session (chốt một quy tắc); (2) đổi test `changed_pinned_certificate_is_reported_distinctly_and_replaces_the_pin_only_on_accept`. `TrustStore::replace_pins` là method bắt buộc, không có default; hiện chưa có impl nào ngoài `svn-ra` nên không phá gì.

### Verdict re-review: **APPROVE**
Cả 4 IMPORTANT đã sửa và có test. Các follow-up không chặn: `ReplacePin` (đã có kế hoạch), M1 redaction lock token/password, rebase lên main mới nhất trước khi merge (merge sạch), chạy `--workspace` đầy đủ khi đĩa hoặc CI cho phép.

Status: DONE
Summary: Head a200f1a đã sửa đủ I1–I4 và M2/M4/M5/M9/M6-doc, có test chứng minh; adapt #43 đúng quy tắc ưu tiên và test changed+mismatch đạt. Gates targeted xanh, đã cargo clean. Verdict APPROVE; còn mở M1 redaction và ReplacePin follow-up.
