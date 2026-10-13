# Red-team: Scope & Complexity Critic + Contract Verifier

Plan: `plans/261013-1410-mvp-parallel-implementation/` · Ngày: 2026-10-13 · Reviewer: code-reviewer
Đường dẫn trong report tính từ repo root `/Users/hxd/Documents/06-Code/OpenSVN/`. `PLAN/` = `plans/261013-1410-mvp-parallel-implementation/`.

## Finding 1: `CommitDriver` chưa được định nghĩa, chiều điều khiển mâu thuẫn, fake không hỗ trợ commit, và remote_edit/copy_location không có engine owner
- **Severity:** Critical
- **Location:** phase-01 §Contract `svn-types` (RaSession::commit); phase-10 §Requirements commit; phase-11 §Requirements commit; plan.md bảng ownership
- **Flaw:** Trait điều khiển commit chỉ có trong một comment, không có chữ ký nào. Phase 01 nói "engine gọi" CommitDriver (push), còn `RaSession::commit(ops: &mut dyn CommitDriver)` thì nghĩa là RA gọi vào driver (pull), và P10 viết "nhận chuỗi thao tác từ CommitDriver". Không ai xác định được thứ tự cây, bên nào sở hữu delta stream, hay copy-from / base checksum đi qua đường nào. Success criteria của `FakeRaSession` không có commit. Bên cạnh đó, `remote_edit`, `copy_location` và `branches_list` (doc 03 §3.4) là commit không qua WC (kiểu svnmucc), nhưng không phase engine nào nhận làm CommitDriver cho chúng. P11 chỉ làm commit từ WC; P10 chỉ làm phía RA.
- **Failure scenario:** Tuần 1 của wave 1, P10 tự nghĩ ra CommitDriver dạng pull-iterator, còn P11 track C (sớm hơn, chạy trên fake) tự nghĩ ra dạng callback-editor. Vì FakeRaSession không có commit, track C không viết được test. Khi tích hợp sẽ phải mở contract PR viết lại cả hai bên. Các màn Browse "new folder/upload/rename/delete/copy" và New branch/tag của P12 chỉ chạy được trên FakeEngine, và tới P13 mới lộ ra là không có ai cài đặt.
- **Evidence:**
  - `PLAN/phase-01-workspace-bootstrap-and-shared-contracts.md:117` — `ops: &mut (dyn CommitDriver + Send), cancel: &CancelToken) -> Result<CommitInfo>;`
  - `PLAN/phase-01-...:121` — `// CommitDriver (engine gọi để phát add/delete/modify/prop theo thứ tự cây; RA chuyển thành PUT/MKCOL/…)`
  - `PLAN/phase-01-...:148` — `FakeRaSession đủ để viết test log, list, get_file, do_update (checkout cây 3 cấp)` (không có commit)
  - `PLAN/phase-10-svn-ra-remote-operations.md:23` — `commit: nhận chuỗi thao tác từ CommitDriver`
  - `PLAN/phase-11-svn-client-working-copy-operations.md:24` — `phát CommitDriver cho RA`
  - `docs/system-design/03-ipc-api-contract-and-data-model.md:141` — `remote_edit | { ops: RemoteOp[], message } (mkdir/rename/delete/copy/upload)`; `:145` `copy_location`
  - `PLAN/phase-12-frontend-feature-screens-wired-to-ipc.md:19` — `thao tác trực tiếp server: new folder, upload, rename, delete, copy`
- **Suggested fix:** P01 phải đưa chữ ký trait vào code thật, ví dụ commit trả về `Box<dyn DynDeltaEditor>` cho client drive (giống `svn_ra_get_commit_editor`; bỏ CommitDriver và dùng lại DeltaEditor đã có), cộng với `FakeRaSession::commit` ghi revision vào repo in-memory. Thêm success criterion "fake commit add/modify/delete/copy". Giao việc `RemoteOp → editor` (khoảng 150 LOC) cho P10, đặt ở `ops/remote_edit.rs`, và ghi rõ trong bảng ownership.

## Finding 2: `apply_textdelta` trả `Box<dyn WindowSink + '_>` không `Send`, và DeltaEditor sync được gọi trong driver async, nên không compile được hoặc làm nghẽn tokio
- **Severity:** High
- **Location:** phase-01 §Delta editor; phase-10 §do_update (send-all); phase-11 checkout/update editor
- **Flaw:** `do_update` là `async` qua `async_trait`, nên future phải `Send`. Ở chế độ send-all, P10 đẩy txdelta inline từ network stream vào `WindowSink`, tức là giữ sink qua các lần `.await` đọc socket. `Box<dyn WindowSink + '_>` không có `+ Send`, nên future không còn `Send` và compile fail. Thêm nữa, DeltaEditor được chủ ý thiết kế SYNC, mà `WcUpdateEditor` (P11) ghi SQLite và pristine trực tiếp, nghĩa là I/O đĩa chạy ngay trên worker tokio. Không phase nào nói tới `spawn_blocking` hay kênh trung gian. `DynDeltaEditor` (lớp object-safe bọc các associated type `Dir`/`File`) cũng chỉ là một cái tên, không có định nghĩa.
- **Failure scenario:** P10 làm send-all trước (bước 3) và gặp lỗi `future cannot be sent between threads safely`. Cách sửa nhanh là buffer toàn bộ txdelta ra file tạm, mất streaming. Hoặc nếu đổi contract thì P11 đang code dở editor phải rebase. Ở WC 100k file, editor sync chặn runtime, nên progress event/cancel của P08 trễ quá mục tiêu "< 1s".
- **Evidence:**
  - `PLAN/phase-01-...:66` — `// Delta editor — tương đương svn_delta_editor_t, SYNC`
  - `PLAN/phase-01-...:79` — `-> Result<Box<dyn WindowSink + '_>>;`
  - `PLAN/phase-01-...:100` — `#[async_trait::async_trait]`; `:114-115` `async fn do_update(... editor: &mut (dyn DynDeltaEditor + Send) ...)`
  - `PLAN/phase-10-...:22` — `send-all: txdelta inline → WindowSink`
  - `PLAN/phase-01-...:154` — `async_trait + object safety phức tạp → giữ trait object-safe, có wrapper DynDeltaEditor.` (không có định nghĩa)
- **Suggested fix:** Chốt một mô hình trong P01 và viết bằng code compile được, kèm test `do_update` với `RecordingEditor`. Cách 1: RA chạy parser trên task riêng và đẩy `UpdateEvent` qua `mpsc` bounded tới thread editor sync (`spawn_blocking`). Cách 2: `WindowSink: Send` và `DynDeltaEditor: Send`, có doc rõ rằng editor không được block. Định nghĩa `DynDeltaEditor` ngay trong P01 (dùng `Box<dyn Any + Send>` hoặc handle id kiểu `DirId(u32)`).

## Finding 3: `svn-types` kéo theo tokio và async-trait, nên CI "không dependency I/O" của P03 chắc chắn đỏ
- **Severity:** High
- **Location:** phase-01 §Contract `svn-types` (`RaSession::get_file`); phase-03 §Non-functional, §Success Criteria
- **Flaw:** P01 tuyên bố svn-types "không phụ thuộc I/O", nhưng `RaSession` dùng `tokio::io::AsyncWrite` và `async_trait`, và workspace deps của P01 có tokio. P03 phụ thuộc svn-types, đồng thời bắt CI kiểm `cargo tree` để không có I/O dependency.
- **Failure scenario:** Ngày đầu P03 thêm `svn-types = { workspace = true }` thì job `cargo tree` báo `tokio` nằm trong cây và CI đỏ. Khi đó hoặc P03 bỏ kiểm tra (đánh mất mục tiêu sans-IO), hoặc phải mở contract PR tách crate, làm cả 9 agent wave 1 phải rebase.
- **Evidence:**
  - `PLAN/phase-01-...:42` — `// crates/svn-types/src/lib.rs — mọi kiểu dùng chung, không phụ thuộc I/O`
  - `PLAN/phase-01-...:107` — `sink: &mut (dyn tokio::io::AsyncWrite + Unpin + Send)`
  - `PLAN/phase-03-svn-protocol-sans-io-dav-layer.md:29` — `không phụ thuộc tokio/reqwest (chỉ bytes, quick-xml, base64, svn-types ...)`
  - `PLAN/phase-03-...:63` — `Không dependency I/O (cargo tree kiểm trong CI).`
- **Suggested fix:** Tách ngay trong P01: `svn-types` chỉ chứa kiểu thuần + DeltaEditor/Reporter/WindowSink (sync); `RaSession` async + fake đặt ở `svn-ra-api` (hoặc `svn-types` với feature `async` mặc định tắt). Ghi rule CI tương ứng.

## Finding 4: `AuthPrompter` nằm trong `svn-ra` (P04), không thuộc contract P01; P08 tham chiếu một "trait tạm theo contract P01" không tồn tại
- **Severity:** High
- **Location:** phase-04 §Architecture (`auth/mod.rs`); phase-08 §Requirements AuthBroker, bước 6; phase-01 §Contract
- **Flaw:** `AuthPrompter`, `Credentials`, `CertInfo` và `CertDecision` được định nghĩa trong crate `svn-ra` do P04 tạo ở wave 1. P08 cũng chạy wave 1, deps chỉ có `[1]`, nhưng lại phải implement trait này và ghi "trước đó trait tạm theo contract P01". Grep cả phase-01 thì không có `AuthPrompter`, `Credentials` hay `CertInfo`. Doc 03 yêu cầu prompt kèm `requestId` và trạng thái "tạm dừng operation", còn chữ ký P04 không có request id, không có cờ "lưu mật khẩu", không có loại auth (basic/negotiate) để AuthBroker ghi `account.method`.
- **Failure scenario:** P08 tự đặt một trait tạm trong `src-tauri`. Đến khi P04 merge, chữ ký khác (`attempt: u8`, `Option<Credentials>`, cert gồm những trường nào) nên P08 phải viết lại AuthBroker. Dialog Sign in/Trust của P12 (wave 2, bước 1) bám theo bản tạm, lại phải sửa thêm lần nữa. Bảng `account.method` không có nguồn dữ liệu.
- **Evidence:**
  - `PLAN/phase-04-transport-auth-and-secure-store.md:36` — `auth/mod.rs            AuthPrompter trait, AuthState theo realm`
  - `PLAN/phase-04-...:43-45` — `async fn credentials(&self, realm: &str, url: &RepoUrl, attempt: u8) -> Option<Credentials>;` / `confirm_certificate(&self, info: &CertInfo) -> CertDecision`
  - `PLAN/phase-08-app-backend-core-services.md:58` — `dùng AuthPrompter của P04 khi merge (trước đó trait tạm theo contract P01).`
  - `PLAN/phase-08-...:7` — `dependencies: [1]`
  - `docs/system-design/03-...:183` — `auth://prompt | { requestId, kind: 'credentials' | 'certificate', realm, url, cert? }`; `:205` `method TEXT NOT NULL CHECK (method IN ('basic','negotiate'))`
- **Suggested fix:** Chuyển `AuthPrompter`, `Credentials { username, password: SecretString, save: bool }`, `CertInfo { host, sha256, subject, issuer, not_after }` và `CertDecision` vào contract P01 (crate async, xem Finding 3). Trả về `AuthOutcome` có method để AuthBroker ghi `account`.

## Finding 5: Contract dữ liệu lệch giữa svn-types, IPC và doc 03 (ErrorCode "map 1-1" bất khả thi; không có kiểu status phía engine)
- **Severity:** High
- **Location:** phase-01 §Contract `svn-types` (ErrorCode, LogEntry, NodeKind) và §Contract `opensvn-ipc`; phase-11 §status; phase-07 M9
- **Flaw:**
  1. `svn_types::ErrorCode` có `Corrupt` và `Svn(u32)`, còn IPC `ErrorCode` không có `corrupt` và cũng không có biến thể mã số, nên câu "map 1-1" là sai.
  2. Engine không có kiểu status trong contract. P11 trả về `FileStatus (doc 03)`, nghĩa là `svn-client` phải phụ thuộc vào `opensvn-ipc` (tầng UI), hoặc P07, P11 và P08 mỗi bên tự định nghĩa một kiểu status. `FileStatus` cần `binary`, `size`, `needsLock`, `lock.mine`, `remote.remoteLock`, `kind: file|dir|symlink`, trong khi `NodeKind` của engine có `None/Unknown` (là kind của các node missing/unversioned).
  3. `LogEntry.date`/`message` là `Option` ở engine nhưng bắt buộc ở IPC.
- **Failure scenario:** Gặp WC hỏng, engine trả `Corrupt`; P08 map sang `internal`, UI không hiện action "cleanup", người dùng không biết phải làm gì. P07 M9 trả struct A, P11 bọc thành struct B, P08 chuyển tiếp sang C: ba lần chuyển đổi, và trường `binary` (cần đọc `svn:mime-type`) không ai tính, nên Changes tab hiển thị diff sai cho file binary.
- **Evidence:**
  - `PLAN/phase-01-...:59-62` — `... DiskFull, PermissionDenied, FileInUse, Cancelled, Corrupt, Internal, Svn(u32) */`
  - `PLAN/phase-01-...:127` — `AppError + ErrorCode (map 1-1 từ svn_types::ErrorCode).`
  - `docs/system-design/03-...:166-169` — `type ErrorCode = 'network' | ... | 'cancelled' | 'internal';` (không có `corrupt`)
  - `docs/system-design/03-...:34-41` — `interface FileStatus { ... needsLock: boolean; remote?: {...}; binary: boolean; size?: number; }`
  - `PLAN/phase-11-...:22` — `trả FileStatus (doc 03) gồm moved_from/to, changelist, needs_lock.`
  - `PLAN/phase-01-...:53` — `pub date: Option<Timestamp>, pub message: Option<String>` vs `docs/system-design/03-...:46` `date: string; message: string;`
- **Suggested fix:** P01 thêm `svn_types::WcStatus` (engine-level, đủ trường cho FileStatus) và một bảng mapping ErrorCode → IPC (thêm `corrupt` vào doc 03 qua lead, hoặc quy định rõ `Corrupt → wc-needs-cleanup`, `Svn(n) → internal + detail`). Thêm test snapshot mapping trong `opensvn-ipc`.

## Finding 6: Plan tự nói phase 5–6 nằm sau MVP nhưng lại kéo hạng mục phase 5–6 vào MVP (format 32 + hydrate, perf ≥30%, externals, mergeinfo, tree conflict)
- **Severity:** Critical
- **Location:** plan.md §Overview và §Acceptance criteria; phase-07 Requirements (format 32, M4 hydration, M10 externals); phase-03 (mergeinfo); phase-11 (upgrade_format, tree conflict); phase-10/13 (bench ≥30%)
- **Flaw:** plan.md khẳng định chỉ chi tiết hóa phase 1–4 của plan tổng. Nhưng doc 02 §1.1 và plan cũ xếp "format 32 pristine-on-demand, tải song song" vào phase 5, "merge/mergeinfo, tree conflict, externals" vào phase 6, còn tiêu chí "checkout nhanh hơn svn ≥30%; status <3s" là success criteria của phase 5 cũ. Plan mới vẫn đưa tất cả vào MVP acceptance và các phase P07/P10/P11/P13. Đây là scope drift ngầm, không có ghi chú quyết định nào.
- **Failure scenario:** P07 track B phải làm hydrate/dehydrate, TEXTBASE_REFS, trigger format 32 và cả bước 0 research, trên đường găng. P11 thêm `upgrade_format` và commit fulltext khi không có pristine. P13 không qua được gate "≥30% nhanh hơn svn" (một mục tối ưu) nên MVP không phát hành dù luồng hằng ngày đã chạy. Ước tính có thể thêm 3–5 tuần trên đường găng.
- **Evidence:**
  - `PLAN/plan.md:19` — `Chi tiết hóa phase 1–4 ... (phase 5–6 cũ — tối ưu repo lớn nâng cao, merge/revision graph — là plan sau MVP).`
  - `docs/system-design/02-architecture-data-flow-and-reliability.md:10` — `| Repo lớn | tải song song, format 32 pristine-on-demand, cache log, lock UX, theo dõi revision mới | 5 |`; `:11` `| Nâng cao | merge/mergeinfo, conflict 3-way, tree conflict, externals, ... | 6 |`
  - `plans/261013-1101-cross-platform-svn-desktop-client/phase-05-large-repo-and-binary-optimization.md:7` — `Format 32 + pristines on demand`; `:28` `Checkout nhanh hơn svn CLI ≥30%; status < 3s`
  - `PLAN/plan.md:110` — `WC 100k file: status lần sau < 3s; checkout nhanh hơn svn CLI ≥ 30%`
  - `PLAN/phase-07-...:27` — `format 32: hydrated, TEXTBASE_REFS, dehydrate/hydrate`; `:43` `M10 externals`
  - `PLAN/phase-03-...:24` — `mergeinfo-report (P6 dùng; MVP parse sẵn)`
  - `PLAN/phase-11-...:27` — `upgrade_format 31→32`
- **Suggested fix:** Không tự ý đảo quyết định của PO (ADR-004 Accepted cho format 32). Trình lead/PO các lựa chọn sau:
  - (a) MVP chỉ format 31 đọc/ghi; format 32 chỉ đọc + từ chối ghi với lỗi rõ; hydrate/upgrade và perf ≥30% chuyển sang plan phase 5.
  - (b) Giữ format 32 nhưng bỏ `upgrade_format` và hạ gate perf xuống "không chậm hơn svn".
  - (c) Giữ nguyên, nhưng phải cộng thời gian vào ước lượng một cách trung thực.
  - Trong mọi trường hợp: xóa `mergeinfo.rs` và M10 externals ghi (chỉ đọc + bỏ qua) khỏi MVP.

## Finding 7: Gold plating rải khắp các phase (encoder delta, nén encoder, LCS dự phòng, HTTP/2, ImageCompare onion/swipe, gallery, soak 8h, bench 50GB/1M rev, 2 kênh release, 5 định dạng gói)
- **Severity:** High
- **Location:** phase-02, phase-04, phase-09, phase-10, phase-13, phase-14
- **Flaw:** Nhiều hạng mục không phục vụ luồng 10.1–10.5 mà vẫn tiêu tốn agent-tuần và trở thành gate success criteria:
  - P02 làm encoder delta "tùy chọn" và encoder v1/v2, trong khi commit chỉ cần fulltext v0.
  - P10 có blame "LCS đơn giản sau feature flag", tức là làm song song một bản diff khác với P05.
  - P04 có HTTP/2 qua ALPN, trong khi research chỉ khuyến nghị HTTP/1.1 pool.
  - P09 làm ImageCompare onion/swipe và gallery có snapshot 2 theme làm gate.
  - P13 có soak 8h, repo 50GB, log 1M revision, self-hosted runner.
  - P14 có kênh stable/beta, MSI + NSIS, AppImage + deb + rpm, DMG universal.
- **Failure scenario:** Agent P02 dành 1 tuần tối ưu encoder delta xdelta-like để "giảm upload", không có consumer nào bắt buộc. P13 bị chặn vì GitHub runner không đủ 50GB phải chờ dựng self-hosted, và gate "Soak 8h" kéo dài vòng sửa lỗi. P14 phải duy trì 7 artifact × ký số cho một đợt phát hành nội bộ.
- **Evidence:**
  - `PLAN/phase-02-svn-delta-svndiff-codec.md:21` — `Encoder: fulltext → svndiff ... — đủ cho commit; tùy chọn delta thật (xdelta-like)`; `:49` `encoder delta đơn giản (match block hash)`
  - `PLAN/phase-10-...:21` — `dùng svn_diff::diff2 khi P05 sẵn (trước đó LCS đơn giản sau feature flag).`
  - `PLAN/phase-04-...:20` — `HTTP/1.1; HTTP/2 nếu ALPN có.` vs `PLAN/reports/from-researcher-to-planner-svn-http-protocol-v2-report.md:167` `HTTP/1.1 pool (max 4, clamp 2–8)`
  - `PLAN/phase-09-frontend-shell-and-design-system.md:49` — `ImageCompare (side-by-side/onion/swipe)`; `:26` `/__gallery`; `:51` `snapshot gallery 2 theme`
  - `PLAN/phase-13-integration-crash-safety-and-performance.md:22` — `repo tổng hợp 100k file / 50GB ... log 1M revision`; `:24` `Soak: app chạy 8h`
  - `PLAN/phase-14-packaging-signing-and-release.md:19` — `MSI (WiX) + NSIS`; `:21` `AppImage + .deb + .rpm`; `:22` `kênh stable / beta`
- **Suggested fix:**
  - Cắt: encoder delta và encoder v1/v2 (giữ decoder đủ 3 version); LCS dự phòng (blame chờ P05 hoặc đặt blame ở wave 3); HTTP/2; onion/swipe (giữ side-by-side; doc 01:83 và system-architecture:53 là quyết định docs nên trình lead); gate snapshot gallery; soak 8h; repo 50GB và 1M rev (dùng 100k file / ~5GB khớp acceptance); kênh beta.
  - Mỗi OS chỉ giữ một định dạng gói: MSI, DMG, AppImage hoặc deb.

## Finding 8: Đường găng 22–26 tuần tính sai: thiếu đoạn P12 phần WC, không có slack, và P11 thực tế phải chờ P07 hoàn tất
- **Severity:** Critical
- **Location:** plan.md §Waves (Đường găng); phase-07/11/12/13 effort
- **Flaw:** Đường găng ghi là P01→P07→P11→P13. Cộng cận trên: 1.5 + 10 + 10 + 4 = 25.5 tuần, đã chạm trần 26 với slack bằng 0. Nhưng P13 phụ thuộc cả P12, và màn WC của P12 chỉ làm được "khi P11 sẵn", nên đường găng thật là P01→P07→P11→P12(WC)→P13. Ngoài ra:
  - P11 track U cần `WcUpdateEditor`, mà editor này dùng M6 work queue và M7 conflicts, là những module cuối cùng của track B P07. Vì vậy câu "P11 bắt đầu sớm" mâu thuẫn với deps `[5, 7, 10]`.
  - P13 gồm 1–2 tuần dogfood cộng gate "0 lỗi chặn trong 1 tuần cuối", lỗi lại phải chuyển về owner của các phase đã đóng.
  - P07 cộng thêm bước 0 research 3–4 ngày, và report research còn nhiều mục UNVERIFIED.
- **Failure scenario:** Chỉ cần P07 trễ 2 tuần (rủi ro chính mà plan tự nhận) là MVP lùi về khoảng tuần 30–32. Lead nhận ra ở tuần ~20, khi 9 agent đã tiêu tốn ngân sách.
- **Evidence:**
  - `PLAN/plan.md:66` — `Đường găng: P01 → P07 → P11 → P13 ... Ước lượng tới MVP: ~22–26 tuần`
  - `PLAN/phase-01-...:6` `effort: "1–1.5 tuần"`; `PLAN/phase-07-...:6` `effort: "8–10 tuần · 2 agent"`; `PLAN/phase-11-...:6` `effort: "8–10 tuần · 2 agent"`; `PLAN/phase-13-...:6` `effort: "3–4 tuần"`; `:25` `Dogfood: 1–2 tuần`
  - `PLAN/phase-13-...:7` — `dependencies: [11, 12]`
  - `PLAN/phase-12-...:20` — `Wave 3 (WC, khi P11 sẵn): tab Changes ...`
  - `PLAN/phase-11-...:7` `dependencies: [5, 7, 10]` vs `:16` `Có thể bắt đầu sớm (cuối wave 2) bằng FakeRaSession + svn-wc đang dở`
  - `PLAN/phase-07-...:51` — `B: M5 → M4 (chờ M2) → M6, M7 (chờ M1, M4, M5) → M9 (chờ M3).`
  - `PLAN/phase-07-...:79` — `Phần lớn rủi ro dự án nằm ở đây`
- **Suggested fix:** Vẽ lại đường găng gồm cả P12-WC, cộng buffer tích hợp ≥ 20%, và báo ước lượng dạng P50/P90 (ví dụ 26/34 tuần). Bỏ các phần ở Finding 6 và 7 khỏi đường găng. Cho P12-WC chạy song song trên FakeEngine ngay từ wave 2 (P08 đã có FakeEngine) để phần đuôi phía sau P11 chỉ còn là wiring.

## Finding 9: Không có bản dùng được nào trước khoảng tuần 20; plan cũ có "bản sớm" sau remote ops thì plan mới bỏ mất
- **Severity:** High
- **Location:** plan.md §Waves; phase-10 (gộp read + update driver + commit driver); phase-14 bước 1
- **Flaw:** Mọi giá trị đến người dùng đều dồn vào wave 4. Plan tổng trước đó có mốc "App desktop (bản sớm): đăng nhập, duyệt repo, xem log/diff/blame, lock" ngay sau remote ops. Plan mới gộp read ops, update driver và commit driver vào cùng một phase P10 (3–4 tuần), nên read-only không có điểm merge hay phát hành riêng. Bước 1 của P14 chỉ build skeleton ("cửa sổ trống").
- **Failure scenario:** Đến tuần ~10, team có 9 crate xanh trên CI nhưng PO chưa có gì để dùng hay góp ý. Lỗi UX Sign in/SSO và hiệu năng WebKitGTK với dữ liệu thật chỉ lộ ra ở P13. Nếu ngân sách bị cắt giữa chừng, dự án không có sản phẩm nào để giao.
- **Evidence:**
  - `plans/261013-1101-cross-platform-svn-desktop-client/phase-02-remote-operations-without-working-copy.md:27` — `App desktop (bản sớm): đăng nhập, duyệt repo, xem log/diff/blame, lock/unlock, tạo branch/tag`
  - `PLAN/phase-10-...:15` — `cài đủ mọi hàm của trait, gồm 2 driver phức tạp nhất: update driver ... và commit driver`
  - `PLAN/phase-14-...:40` — `Bản build không ký 3 OS từ skeleton (wave 2)`
  - `PLAN/plan.md:54` — `├─► Wave 4: P13 ─► release`
- **Suggested fix:** Tách P10 thành P10a (open/read/log/locks/file_revs) và P10b (update/commit driver). Thêm mốc M1 "Read-only client" = P01+P03+P04+P10a+P08 (RealEngine remote)+P12 remote, phát hành nội bộ qua P14 vào khoảng tuần 8–10, với gate dogfood Browse/History/Lock. Mốc M2 "checkout + status + commit format 31".

## Finding 10: Phase quá lớn so với một agent/PR: P07, P11 8–10 tuần, P12 khoảng 25 feature, P01 1–1.5 tuần phải dựng cả một "mini svn server" fake
- **Severity:** High
- **Location:** phase-01 bước 3 và Success Criteria; phase-07, phase-11 (2 track/crate); phase-12 Architecture; plan.md quy tắc 7
- **Flaw:**
  - P07 và P11 mỗi phase là 2 agent × 8–10 tuần trên cùng một crate, facade do một track sở hữu, track kia "mở PR nhỏ". Không có mốc merge trung gian hay success criteria theo module, mà quy tắc lại là "kết thúc phase" mới báo Status.
  - P12 có khoảng 25 thư mục feature cùng 7 trang Settings, giao cho "1–2 agent" mà không có ranh giới phân chia.
  - P01 trong 1–1.5 tuần phải làm: toàn bộ contract, `FakeRaSession` drive `do_update` một cây 3 cấp (về bản chất là một server delta editor in-memory), toàn bộ kiểu IPC kèm snapshot, stub mọi command, tauri-specta, CI 3 OS có `pnpm tauri build`. Mọi phase khác đều chờ P01.
- **Failure scenario:** P01 trễ 1 tuần thì 9 agent ngồi chờ. P07 track B có PR facade bị nghẽn review và rebase hằng ngày. Một agent P12 đầy context window sau khoảng 10 feature và bắt đầu lệch design-guidelines.
- **Evidence:**
  - `PLAN/phase-01-...:6` — `effort: "1–1.5 tuần · 1 agent"`; `:138` `FakeRaSession (repo in-memory: cây + log + lock), RecordingEditor ...`; `:146` `pnpm tauri build --debug thành công trên 3 OS`
  - `PLAN/phase-01-...:15` — `để 8 agent wave 1 code độc lập` vs `PLAN/plan.md:61` `| 1 | ... | 9 |`
  - `PLAN/phase-07-...:49` — `API công khai WcDb (facade) | A sở hữu, B thêm hàm qua PR nhỏ`
  - `PLAN/phase-11-...:39` — `lib.rs (facade SvnClient) ... | U sở hữu; C thêm hàm qua PR nhỏ`
  - `PLAN/phase-12-...:6` — `effort: "6–7 tuần · 1–2 agent"`; `:29-31` danh sách khoảng 25 feature folder; `:19` `Settings (7 trang ...)`
  - `PLAN/plan.md:98` — `Kết thúc phase: cập nhật checkbox ..., báo Status`
- **Suggested fix:**
  - Tách P07 thành 07a (skel + schema + nodes đọc), 07b (pristine + translate + workqueue), 07c (ghi op_* + conflicts + status); P11 tách thành 11a (checkout/status/update sạch), 11b (commit/revert/add/delete), 11c (merge/conflict/move/lock). Mỗi sub-phase có PR và gate compat riêng.
  - Tách P12 thành 12a (remote/auth/settings) và 12b (changes/dialogs/conflicts), có owner_paths riêng.
  - P01 chia hai bước: 01a contract (types + trait, chưa cần fake drive update), tức là mở khóa wave 1 ngay; 01b fake + IPC + CI, chạy song song với tuần đầu wave 1.

## Concern bị bác (đã kiểm chứng)
- **Cắt Location mode:** bác bỏ. Repo thực tế của người dùng không có layout (`docs/system-design/03-...:23-24` `branch: BranchRef | null; // null khi repo không có layout → UI dùng chế độ Location`, ví dụ `/2. Sản xuất/2. Phát triển`; luồng `docs/system-design/01-...:330`). Location mới là chế độ mặc định cần thiết. Phần có thể hoãn là Branch mode + `layout_detect` custom `/{project}/trunk` (`PLAN/phase-08-...:26`), và việc này cần PO quyết.
- **Cắt blame:** bác bỏ ở mức hạng mục, vì doc 02 xếp blame vào phase Remote (`docs/system-design/02-...:7`). Chỉ cắt bản LCS dự phòng (Finding 7).

## Unresolved questions
1. PO có chấp nhận MVP chỉ ghi format 31 (format 32 chuyển sang plan phase 5) không? Đây là quyết định ADR-004, reviewer không tự đảo.
2. Gate perf "≥ 30% nhanh hơn svn" là yêu cầu cứng của MVP nội bộ hay mục tiêu phase 5?
3. `remote_edit`/`copy_location` có thuộc MVP không? Nếu có, engine owner là P10 hay P11?
4. GSSAPI/Kerberos macOS/Linux: plan cũ ghi "mức spike", plan mới đưa vào cross-krb5 đầy đủ (`PLAN/phase-04-...:22`), trong khi acceptance chỉ yêu cầu Basic trên macOS/Linux (`PLAN/plan.md:108`). Có cắt GSSAPI khỏi MVP không?
