# Red-team review: Assumption Destroyer + Scope Auditor

Date: 2026-10-13 · Reviewer: code-reviewer · Target: `plans/261013-1410-mvp-parallel-implementation/` (plan.md + phase-01..14)
Method: đọc toàn bộ plan/phase, 2 report research, ADR-003/004, doc 02/03; grep đối chiếu; 3 WebSearch + 1 WebFetch (svn 1.15, cross-krb5, keyring v4).

## Finding 1: Toàn bộ chiến lược compat dựa trên "svn 1.15 đã phát hành" — chưa xác minh, nhiều khả năng sai
- **Severity:** Critical
- **Location:** ADR-004 Context; plan.md Acceptance; phase-06 Requirements/Risk; phase-07 Requirements; phase-05 Success Criteria
- **Flaw:** Plan coi svn 1.15 (format 32, pristines-on-demand) là bản release để làm oracle, build image Docker, sinh fixture, golden byte-for-byte. Trang release notes chính thức của Apache (subversion.apache.org/docs/release-notes/1.15.html) vẫn ghi "Subversion 1.15 has not been released yet"; news archive mới nhất tìm được là 1.14.5 (2024-12-08). Research tự thừa nhận chỉ đọc trunk, chưa đối chiếu tag 1.15.x. Thêm nữa, report wc tự mâu thuẫn: dòng 71 nói 1.15 tự nâng 31→32, dòng 163 nói checkout mặc định 31 — nếu 1.15 tự nâng, kịch bản compat "svn15 thao tác rồi svn14 đọc" sẽ hỏng.
- **Failure scenario:** P06 "build từ source" lấy trunk/branch 1.15.x chưa đóng băng → fixture format 32 + golden diff/merge sinh từ bản pre-release; P07 chốt schema theo trunk; khi 1.15.0 thật ra (hoặc không bao giờ ra trong thời gian MVP) schema/hành vi khác → toàn bộ golden phải tái sinh, hoặc người dùng (TortoiseSVN 1.14) không mở được WC format 32 mà UI quảng cáo. VisualSVN Server thực tế có thể bundle 1.14 → tính năng format 32 vô dụng ở khách hàng duy nhất.
- **Evidence:**
  - `docs/adr/adr-004-working-copy-format.md:10` — "Subversion 1.15 đã phát hành:"
  - `reports/from-researcher-to-planner-libsvn-wc-db-format-report.md:18` — "**Not verified**: 1.14.x and 1.15.x release tags (trunk used as proxy)"
  - `reports/from-researcher-to-planner-libsvn-wc-db-format-report.md:71` — "Upgrades from 31 -> 32 happen automatically in 1.15 clients"
  - `reports/from-researcher-to-planner-libsvn-wc-db-format-report.md:163` — "`svn checkout` creates 31 unless `--store-pristine=no`"
  - `phase-06-compat-test-harness-and-fixtures.md:64` — "Package svn 1.15 chưa có trên distro → build từ source"
  - `phase-05-svn-diff-text-diff-and-diff3.md:50` — "khớp byte-for-byte với svn 1.15"
- **Suggested fix:** Bước 0 của P01 (không phải P07): xác minh trạng thái release 1.15 + version mod_dav_svn trong VisualSVN đang chạy. Nếu 1.15 chưa release: oracle chính = 1.14.x (format 31), format 32 chuyển sang post-MVP hoặc feature flag "experimental", pin commit hash của 1.15 branch trong Dockerfile. Giải quyết mâu thuẫn auto-upgrade trong report trước khi viết compat scenario.

## Finding 2: Thiết kế đọc song song dựa trên WAL cho `.svn/wc.db` mâu thuẫn với yêu cầu "journal mode giống svn"
- **Severity:** Critical
- **Location:** ADR-003 bảng lựa chọn (rusqlite … WAL); doc 02 §WcActor; phase-07 Non-functional; phase-08 WcActor ("lệnh đọc song song")
- **Flaw:** ADR-003 và doc 02 chốt WAL cho DB working copy, WcActor thiết kế "đọc song song với snapshot WAL" trong khi đang ghi. Phase-07 lại yêu cầu journal mode giống svn và report đánh dấu UNVERIFIED. libsvn_wc (theo hiểu biết hiện có) dùng rollback journal mặc định (+ tùy chọn exclusive-locking), không WAL. WAL là thuộc tính lưu bền trong file DB và tạo `-wal`/`-shm`; không chạy trên share mạng/SMB.
- **Failure scenario:** (a) Engine bật WAL → wc.db đổi header, TortoiseSVN/svn mở WC trên ổ mạng hoặc khi engine còn giữ `-shm` → lỗi lock/"database is locked"; vi phạm Acceptance "không cần cleanup". (b) Engine giữ rollback journal cho đúng svn → giả định "đọc song song khi ghi" của P08 sụp: StatusService (watcher) đọc trong lúc update ghi → SHARED lock chặn commit của writer → `SQLITE_BUSY` giữa update, đúng lúc crash-safety quan trọng nhất.
- **Evidence:**
  - `docs/adr/adr-003-rust-core-library-stack.md:30` — "`rusqlite` (feature `bundled`), WAL"
  - `docs/system-design/02-architecture-data-flow-and-reliability.md:67` — "**lệnh đọc** (status, diff, log) chạy song song với snapshot đọc SQLite (WAL)"
  - `phase-07-svn-wc-database-and-pristine-store.md:34` — "journal mode / busy timeout **giống svn** (xác minh ở bước 0)"
  - `reports/from-researcher-to-planner-libsvn-wc-db-format-report.md:173` — "UNVERIFIED which mode svn uses"
  - `phase-08-app-backend-core-services.md:20` — "lệnh ghi tuần tự, lệnh đọc song song"
- **Suggested fix:** Xác minh `svn_sqlite.c` NGAY trong P01 (ảnh hưởng contract P08). Sửa ADR-003/doc 02: WAL chỉ cho `app.sqlite`/`cache.sqlite`. WcActor: đọc wc.db phải đi qua cùng actor hoặc có busy-retry + status đọc từ cache RAM khi đang có lệnh ghi; thêm test đồng thời status-trong-update vào P08/P13.

## Finding 3: Contract `svn-types` kéo tokio/async-trait vào crate "sans-IO" — CI check của P03 chắc chắn đỏ
- **Severity:** High
- **Location:** phase-01 Contract `svn-types` (RaSession); phase-03 Non-functional + Success Criteria
- **Flaw:** `svn-types` tuyên bố "không phụ thuộc I/O" nhưng `RaSession::get_file` nhận `tokio::io::AsyncWrite` và trait dùng `#[async_trait]`. P03 phụ thuộc `svn-types` và có tiêu chí "Không dependency I/O (cargo tree kiểm trong CI)" + "không phụ thuộc tokio/reqwest". P02 (svn-delta) cũng dùng `WindowSink`/`TxDeltaWindow` từ svn-types → svn-delta cũng kéo tokio.
- **Failure scenario:** Ngày đầu wave 1, P03 thêm check `cargo tree -p svn-protocol | grep tokio` → fail; phải mở contract PR tách crate (vd `svn-ra-api`) → toàn bộ 8 agent rebase đúng lúc đang code bằng fake — chính tình huống "contract churn" mà P01 được dựng để tránh.
- **Evidence:**
  - `phase-01-workspace-bootstrap-and-shared-contracts.md:42` — "mọi kiểu dùng chung, không phụ thuộc I/O"
  - `phase-01-workspace-bootstrap-and-shared-contracts.md:107` — "sink: &mut (dyn tokio::io::AsyncWrite + Unpin + Send)"
  - `phase-03-svn-protocol-sans-io-dav-layer.md:29` — "không phụ thuộc tokio/reqwest (chỉ `bytes`, `quick-xml`, `base64`, `svn-types`…)"
  - `phase-03-svn-protocol-sans-io-dav-layer.md:63` — "Không dependency I/O (cargo tree kiểm trong CI)"
- **Suggested fix:** P01 tách: `svn-types` (data + sync trait DeltaEditor/Reporter/WindowSink, zero I/O dep) và `svn-ra-api` (RaSession async + FakeRaSession). Hoặc `RaSession` nhận sink trait tự định nghĩa, tokio sau feature.

## Finding 4: Update driver skelta — hoặc tải tuần tự (giết mục tiêu ≥30%), hoặc vi phạm thứ tự editor mà contract không định nghĩa
- **Severity:** High
- **Location:** phase-01 DeltaEditor; phase-10 do_update + sơ đồ luồng skelta + Non-functional; report HTTP §2
- **Flaw:** (a) Sơ đồ P10: `CloseFile ─► chờ GET xong ─► apply_textdelta ─► close_file`. Trong skelta, `S:fetch-file` và `close-file` liền nhau → chờ GET ở CloseFile = tuần tự hóa toàn bộ download; "GET song song ≤ N" không xảy ra. Cách ra_serf làm là hoãn close_file (editor svn cho phép file baton mở lâu hơn), nhưng contract P01 chỉ nói "đẩy theo thứ tự (driver đảm bảo tuần tự)", không quy định có được close_directory trước close_file con không — P11 WcUpdateEditor và RecordingEditor viết theo giả định khác nhau. (b) Mỗi file đi qua file tạm của P10 → pristine tmp của P07 → working file = 3 lần ghi, so với svn. (c) Report khẳng định GET skelta trả fulltext "no delta"; ra_serf thực tế gửi `X-SVN-VR-Base` + `Accept-Encoding: svndiff*` để nhận delta so với base khi update file đã có — plan không có, update file binary lớn sẽ chậm hơn svn CLI chứ không nhanh hơn. (d) svn CLI cũng dùng 4 kết nối mặc định (report §6) → "4 kết nối nhanh hơn ≥30%" không có cơ chế vượt trội nào được nêu.
- **Failure scenario:** Bench P10/P13 checkout 1GB: OpenSVN tải từng file một → chậm hơn svn; sửa bằng cách hoãn close_file → P11 editor (viết theo thứ tự chặt) panic/ghi sai NODES khi close_directory đến trước close_file; RecordingEditor golden phải viết lại.
- **Evidence:**
  - `phase-10-svn-ra-remote-operations.md:43` — "CloseFile ─► chờ GET xong ─► editor.apply_textdelta (fulltext window) ─► close_file(checksum)"
  - `phase-10-svn-ra-remote-operations.md:26` — "checkout repo fixture 1GB với 4 kết nối nhanh hơn `svn checkout` … ≥ 30%"
  - `phase-01-workspace-bootstrap-and-shared-contracts.md:66` — "SYNC, đẩy theo thứ tự (driver đảm bảo tuần tự)"
  - `reports/from-researcher-to-planner-svn-http-protocol-v2-report.md:79` — "decode body directly as file content (no delta). Max parallel GETs ≤ `http-max-connections`"
  - `reports/from-researcher-to-planner-svn-http-protocol-v2-report.md:148` — "`http-max-connections` default **4**"
- **Suggested fix:** Contract P01 ghi rõ quy tắc thứ tự (file baton có thể đóng trễ tới close_edit / trước close_directory cha — chọn một) + test RecordingEditor theo quy tắc đó. P10: hàng đợi file chờ, close theo thứ tự hoàn tất trong phạm vi dir chưa đóng. P10 spike delta-GET (`X-SVN-VR-Base`). Ghi rõ nguồn của "≥30%" (vd ghi thẳng vào pristine, ít fsync) hoặc hạ thành mục tiêu "không chậm hơn".

## Finding 5: Auth giả định "401 → prompt → retry" không áp dụng được cho body stream; Negotiate/NTLM qua pool reqwest chưa khả thi
- **Severity:** High
- **Location:** phase-04 Requirements (send stream, Auth, Retry, HTTP/2); ADR-003 Auth
- **Flaw:** (a) PUT/REPORT update-report có body stream (không buffer). 401 (session hết hạn, Basic chưa preemptive, Negotiate) trả về sau khi body đã tiêu → không replay được; plan không có chiến lược "xác thực trước bằng OPTIONS/HEAD" hay body có thể tua lại. Retry chỉ cho idempotent, nhưng auth-retry cho PUT/MERGE không được nói tới. (b) ADR-003 hứa NTLM fallback qua SSPI; `cross-krb5` mô tả là "safe interface for Kerberos 5" — không có NTLM/SPNEGO wrapper rõ ràng. Khi người dùng truy cập VisualSVN qua IP/CNAME không có SPN (rất phổ biến), Kerberos fail → không có NTLM → SSO Windows không hoạt động, mất tiêu chí Acceptance. NTLM lại gắn theo kết nối TCP, trái với pool reqwest không cho chọn kết nối. (c) "HTTP/2 nếu ALPN có" không tương thích Negotiate theo kết nối.
- **Failure scenario:** Commit 500 file: credential hết hạn giữa chừng → PUT thứ 200 nhận 401 sau khi đã stream 300MB → transport không retry được → commit fail giữa txn; hoặc AD user trên laptop domain truy cập `https://svn/` (short name) → luôn rơi về prompt Basic, "SSO" Acceptance đỏ.
- **Evidence:**
  - `phase-04-transport-auth-and-secure-store.md:19` — "body stream (không buffer cả body) cả chiều gửi lẫn nhận"
  - `phase-04-transport-auth-and-secure-store.md:22` — "401 → `AuthPrompter::credentials(realm)` tối đa 3 lần"
  - `phase-04-transport-auth-and-secure-store.md:20` — "HTTP/2 nếu ALPN có"
  - `docs/adr/adr-003-rust-core-library-stack.md:25` — "NTLM qua SSPI trên Windows nếu Kerberos thất bại"
  - `reports/from-researcher-to-planner-svn-http-protocol-v2-report.md:144` — "NTLM also possible via SSPI on Windows … **[exact header samples not captured; verify with fixture]**"
  - `plan.md:108` — "Windows SSO (Negotiate/SSPI)"
- **Suggested fix:** P04: xác thực sớm bằng OPTIONS trên cùng kết nối trước request có body; Basic preemptive sau khi biết realm; body PUT từ file (có thể mở lại) thay vì stream một lần; auth-retry riêng cho non-idempotent khi body replayable. Spike tuần 1: `cross-krb5` vs crate `windows`/SSPI trực tiếp với package "Negotiate"; tắt HTTP/2 khi dùng Negotiate; định nghĩa connection-affinity (có thể phải xuống hyper).

## Finding 6: DoD "không dependency C ngoài rusqlite" bị chính stack đã chọn vi phạm; `keyring` v4 đã đổi kiến trúc
- **Severity:** High
- **Location:** plan.md Definition of Done; phase-04 secure-store + Negotiate; ADR-003 Secret
- **Flaw:** `cross-krb5` trên macOS/Linux bind GSSAPI hệ thống (C, libgssapi_krb5); Linux Secret Service qua `keyring` thường kéo libdbus (C) trừ khi chọn backend zbus; Tauri Linux link WebKitGTK. DoD cấm mọi C dep ngoài rusqlite trừ khi sửa ADR-003 → hoặc mọi PR P04/P08 bị chặn, hoặc DoD bị lờ. Ngoài ra `keyring` 4.x đã tách API sang `keyring-core` + crate store riêng từng OS (docs.rs keyring 4.0.0: "now just a sample code crate"; 4.1.3 bị yank) — "keyring v4" trong plan không xác định API nào.
- **Failure scenario:** `cargo deny` (ban C/OpenSSL) hoặc review DoD fail PR P04; Linux build AppImage thiếu libgssapi lúc runtime → app crash khi bật feature negotiate; secure-store viết trên API v1 wrapper bị deprecate.
- **Evidence:**
  - `plan.md:104` — "không thêm dependency có C ngoài `rusqlite` (bundled) mà không cập nhật ADR-003"
  - `phase-04-transport-auth-and-secure-store.md:25` — "Linux Secret Service (keyring v4)"
  - `docs/adr/adr-003-rust-core-library-stack.md:35` — "`keyring` v4 (Windows Credential Manager, macOS Keychain, Linux Secret Service)"
  - `phase-01-workspace-bootstrap-and-shared-contracts.md:27` — "deny.toml … cấm crate có OpenSSL"
- **Suggested fix:** P01 cập nhật ADR-003 liệt kê ngoại lệ C hợp lệ (GSSAPI dynamic, WebKitGTK); chốt `keyring-core` + store crate cụ thể (Linux: backend zbus pure-Rust); P14 khai báo dependency runtime libgssapi/dbus cho .deb/.rpm/AppImage.

## Finding 7: Bảng ownership có lỗ hổng và chồng lấn thật — "mỗi path chỉ 1 phase ghi" không đúng
- **Severity:** High
- **Location:** plan.md File ownership; phase-01/04/06/08/09/10/13/14 owner_paths
- **Flaw:**
  1. `crates/svn-ra/`: P01 tạo crate (lib.rs rỗng), P04 "tạo crate" và sở hữu `src/lib.rs`; P10 chỉ sở hữu `src/ops/**` nhưng phải thêm `pub mod ops` vào lib.rs và thêm deps (`svn-delta`, `svn-diff`) vào `crates/svn-ra/Cargo.toml` — file này không ai sở hữu trong bảng.
  2. `apps/opensvn/src/ipc/bindings.ts`: thuộc P09 (`src/ipc/`), nhưng được sinh lại mỗi khi P08 đổi command → P08 ghi vào path của P09.
  3. `tests/compat/` của P06 vs `tests/compat/scenarios/{protocol,wc,ra,client}/` do P03/P07/P10/P11 sở hữu — bảng không ghi ngoại lệ; `xtask/` có trong phase-06 nhưng không có trong bảng.
  4. P13 owner_paths có `docs/` trong khi bảng gán `docs/` cho lead.
  5. `tauri.conf.json` chia theo key JSON giữa P08 và P14 → cùng file, xung đột rebase hằng ngày.
  6. P10 giao việc cho P08 ("ghi tên txn đang mở vào file tạm của app") nhưng P08 không có requirement này; P13 cần hook kill trong P07 — phụ thuộc chéo không nằm trong dependencies.
  7. P12 E2E chạy trên Windows CI với server Docker P06 — runner Windows hosted không chạy Linux container; workflow nằm ở `ci.yml` do P01 sở hữu.
- **Failure scenario:** P10 không compile module ops mà không sửa file của P04 → chờ PR chéo; P08 và P09 cùng sửa `bindings.ts` → conflict liên tục, CI "bindings không lệch" đỏ luân phiên.
- **Evidence:**
  - `phase-01-workspace-bootstrap-and-shared-contracts.md:31` — "svn-ra/ … (lib.rs rỗng + README 1 dòng; owner phase điền)"
  - `plan.md:77` — "`crates/svn-ra/src/transport/**`, `src/auth/**`, `src/session.rs` | P04 | P04 tạo crate `svn-ra`"
  - `phase-04-transport-auth-and-secure-store.md:31` — "lib.rs pub mod transport, auth, session (ops do P10 thêm)"
  - `phase-10-svn-ra-remote-operations.md:9` — `owner_paths: ["crates/svn-ra/src/ops/**", "tests/compat/scenarios/ra/"]`
  - `phase-08-app-backend-core-services.md:49` — "`apps/opensvn/src/ipc/bindings.ts` chỉ qua `pnpm gen:ipc`"
  - `phase-09-frontend-shell-and-design-system.md:9` — `owner_paths: ["apps/opensvn/src/{app,components,ipc,styles,lib}/", …]`
  - `plan.md:81` — "`docker/`, `tests/compat/`, `fixtures/`, `.github/workflows/compat.yml` | P06"
  - `phase-13-integration-crash-safety-and-performance.md:9` — `"docs/ (đề xuất qua PR cho lead)"` vs `plan.md:89` — "`docs/` | Lead (controller)"
  - `phase-10-svn-ra-remote-operations.md:69` — "ghi tên txn đang mở vào file tạm của app (P08)"
  - `phase-12-frontend-feature-screens-wired-to-ipc.md:45` — "E2E (`tauri-driver`, Windows/Linux CI) … với server Docker P06"
- **Suggested fix:** Bảng ownership thêm dòng: `crates/svn-ra/{Cargo.toml,src/lib.rs}` = P04, P10 thêm `pub mod ops` trong PR P01 skeleton luôn; `bindings.ts` = generated, owner P08, P09 chỉ đọc; liệt kê `tests/compat/scenarios/<phase>/` và `xtask/`; bỏ `docs/` khỏi P13; tách cấu hình bundle/updater P14 sang `tauri.bundle.conf.json` (Tauri hỗ trợ merge config `--config`); thêm requirement tương ứng vào P07/P08; Windows E2E dùng server từ xa hoặc chỉ Linux.

## Finding 8: P01 gánh quá tải trong 1–1.5 tuần nhưng phải "khóa" contract còn để dạng comment
- **Severity:** High
- **Location:** phase-01 effort, Requirements, Contract (đoạn comment), Implementation Steps 3–6
- **Flaw:** Một agent trong 1–1.5 tuần phải: 9 crate, CI 3 OS (kèm `pnpm tauri build --debug` mỗi OS), Tauri skeleton + stub TẤT CẢ command doc 03, toàn bộ type IPC + snapshot, `FakeRaSession` in-memory đủ `do_update` checkout 3 cấp + lock + log, `RecordingEditor`. Trong khi đó `LogRequest`, `UpdateRequest`, `FileRevHandler`, `CommitDriver`, `DynDeltaEditor` chỉ là comment — `DynDeltaEditor` (object-safe hóa trait có associated type `Dir/File` + `Box<dyn WindowSink + '_>` borrow `&mut self`) là phần khó nhất và chưa có thiết kế. Mục tiêu "contract đủ để không đổi trong 2 tuần đầu" chỉ phủ 2/8–10 tuần của P07/P11. `tauri-specta`/`specta` 2.x lâu nay ở trạng thái RC — chưa pin version.
- **Failure scenario:** P01 trễ 1 tuần → 9 agent ngồi chờ (wave 1 phải chờ P01 merge); hoặc merge contract nửa vời → P03 (update events), P10 (CommitDriver), P11 (DeltaEditor) đều mở contract PR trong tuần 2–3, lead duyệt 24h mỗi PR, mọi phase rebase.
- **Evidence:**
  - `phase-01-workspace-bootstrap-and-shared-contracts.md:6` — `effort: "1–1.5 tuần · 1 agent"`
  - `phase-01-workspace-bootstrap-and-shared-contracts.md:19` — "contract đủ để P02–P09 không cần đổi trong 2 tuần đầu"
  - `phase-01-workspace-bootstrap-and-shared-contracts.md:120-121` — "// LogRequest, UpdateRequest …, FileRevHandler, // CommitDriver (…), DynDeltaEditor (object-safe wrapper)."
  - `phase-01-workspace-bootstrap-and-shared-contracts.md:148` — "`FakeRaSession` đủ để viết test `log`, `list`, `get_file`, `do_update` (checkout cây 3 cấp)"
  - `plan.md:61` — "| 1 | P02, P03, …, P08, P09 | 9 | P01 merge vào `main` |"
- **Suggested fix:** Tách P01 thành P01a (workspace + svn-types data types + DeltaEditor/DynDeltaEditor/CommitDriver đầy đủ, merge trong 1 tuần, mở khóa P02/P03/P05/P06/P07) và P01b (Tauri/IPC/CI desktop, mở khóa P08/P09). Viết spike compile `DynDeltaEditor` + 1 driver giả trước khi tuyên bố khóa. Pin `specta`/`tauri-specta` version chính xác.

## Finding 9: Tiêu chí "byte-for-byte" đặt ở nơi thuật toán/nguồn được chọn không thể bảo đảm
- **Severity:** Medium
- **Location:** phase-05 Requirements + Success Criteria; phase-07 Requirements (schema "giống hệt") + Success Criteria
- **Flaw:** (a) P05 dùng `imara-diff` histogram nhưng yêu cầu ≥50 diff2 + ≥40 merge3 khớp byte-for-byte với libsvn_diff (LCS riêng, hunk boundary khác) — chính Risk của phase thừa nhận lệch; không có kế hoạch nếu phải viết lại LCS (effort 2 tuần không tính). (b) P07 yêu cầu `.schema` engine == svn và `sqlite_stat1` "giống hệt". SQLite lưu nguyên văn câu CREATE; svn tạo schema từ SQL đã qua bước biến đổi lúc build (comment bị lược), và WC format 32 tạo bằng upgrade 31→32 có text `ALTER TABLE ADD COLUMN` khác với tạo mới. Report chỉ đòi "modulo whitespace".
- **Failure scenario:** Golden đỏ hàng loạt ở P05 tuần 2 → agent đổi sang tự viết LCS, trễ P11 (merge3 là đầu vào update có sửa local). P07 copy SQL thô từ `wc-metadata.sql` → `.schema` lệch comment/whitespace → Success Criteria không bao giờ đạt dù hành vi đúng.
- **Evidence:**
  - `phase-05-svn-diff-text-diff-and-diff3.md:19` — "dựa `imara-diff` (histogram)"
  - `phase-05-svn-diff-text-diff-and-diff3.md:56` — "imara-diff cho hunk khác LCS của svn → diff3 có thể dùng LCS riêng nếu golden lệch"
  - `phase-07-svn-wc-database-and-pristine-store.md:22` — "schema, trigger, index, `sqlite_stat1` **giống hệt** svn tạo (so `.schema`)"
  - `phase-07-svn-wc-database-and-pristine-store.md:59` — "SQL copy từ `wc-metadata.sql` của tag 1.15.x"
  - `reports/from-researcher-to-planner-libsvn-wc-db-format-report.md:157` — "must equal svn's byte-for-byte (modulo whitespace)"
- **Suggested fix:** P05: quyết định LCS-port-từ-libsvn_diff cho merge3 ngay từ đầu (imara chỉ cho hiển thị); đo lại effort. P07: so sánh schema theo cấu trúc (`PRAGMA table_info/index_list/trigger SQL đã chuẩn hóa`), tạo WC 32 đúng cách svn làm (xác minh tạo mới vs upgrade).

## Finding 10: Ước lượng 22–26 tuần không khớp số học đường găng và các mục chưa tính
- **Severity:** Medium
- **Location:** plan.md Waves (Đường găng); phase-07/11/13 effort; phase-13 Dogfood
- **Flaw:** Đường găng P01 (1–1.5) → P07 (8–10) → P11 (8–10) → P13 (3–4) = 20–25.5 tuần, chưa tính: bước 0 research P07 (3–4 ngày, chặn M5/M6/M7), dogfood 1–2 tuần + "0 lỗi chặn trong 1 tuần cuối", vòng sửa lỗi do P13 trả về P07/P11 (owner đã kết thúc → lead phân công), contract PR churn. P11 "bắt đầu sớm cuối wave 2" nhưng cần WcDb API của P07 đã merge — phần lớn API ghi (`op_*`, workqueue, conflicts) đến cuối P07. Không có buffer. Ngoài ra P13 bench repo 50GB cần self-hosted runner chưa có.
- **Failure scenario:** Kịch bản thực tế: 1.5 + 0.7 + 10 + 10 + 4 + 2 dogfood + 2 fix loop ≈ 30 tuần; team cam kết 26 với PO.
- **Evidence:**
  - `plan.md:66` — "**Đường găng:** P01 → P07 → P11 → P13 … Ước lượng tới MVP: **~22–26 tuần**"
  - `phase-07-svn-wc-database-and-pristine-store.md:6` — `effort: "8–10 tuần · 2 agent (track A + track B)"`
  - `phase-11-svn-client-working-copy-operations.md:6` — `effort: "8–10 tuần · 2 agent …"`
  - `phase-13-integration-crash-safety-and-performance.md:25` — "**Dogfood**: 1–2 tuần dev nội bộ"
  - `phase-13-integration-crash-safety-and-performance.md:55` — "self-hosted runner (máy Mac/Linux của team) cho bench lớn"
- **Suggested fix:** Ghi đường găng có số (cộng dồn từng phase + research + dogfood + fix loop), báo PO dải 26–32 tuần hoặc cắt phạm vi (format 32, blame, Browse thao tác server, 3-way resolver) ra khỏi MVP; xác định milestone "P07 API ghi tối thiểu" ngày cụ thể để P11 bắt đầu.

## Scope audit summary
- Thiếu owner: `crates/svn-ra/Cargo.toml`, `xtask/`, `apps/opensvn/src-tauri/icons/` (P14 có, bảng không), root `README.md`/`.gitignore` (P01 sửa, bảng không ghi).
- Chồng lấn: `bindings.ts` (P08/P09), `tauri.conf.json` (P08/P14), `docs/` (lead/P13), `tests/compat/` (P06 vs P03/P07/P10/P11).
- Claim dựa UNVERIFIED trong report mà plan coi là chắc: Content-Type PUT, lock-conflict status, journal mode, keyword rules, conflict skel grammar, 1.15 tag equality, svndiff1 layout, http-bulk-updates option, server txn GC.

## Unresolved questions
1. VisualSVN Server nội bộ đang chạy bản nào (mod_dav_svn 1.14 hay 1.15)? Quyết định có cần format 32 trong MVP.
2. Người dùng truy cập VisualSVN bằng FQDN có SPN hay short name/IP? Quyết định có cần NTLM.
3. PO chấp nhận dải thời gian > 26 tuần hay cắt phạm vi?

Status: DONE
