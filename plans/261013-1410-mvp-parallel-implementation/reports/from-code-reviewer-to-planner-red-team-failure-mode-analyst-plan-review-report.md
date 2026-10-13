# Red-team plan review — Failure Mode Analyst (Murphy's Law) / Flow Tracer

Plan: `plans/261013-1410-mvp-parallel-implementation/plan.md` + phase-01..14. Date: 2026-10-13.
Flows traced: checkout, update có sửa local, commit, crash recovery, auth prompt, cancel, real-engine hand-off.

## Finding 1: Update có sửa local không atomic qua work queue — crash giữa "bump NODES base" và merge3 làm mất/đảo ngược thay đổi
- **Severity:** Critical
- **Location:** Phase 11 Requirements (update/switch); docs 02 §2.3 Update; Phase 7 Work queue
- **Flaw:** Luồng update ghi NODES base (rev mới) và chạy work queue TRƯỚC, rồi mới diff3 cho file sửa local. Không phase nào yêu cầu kết quả merge3 (file tạm merged + conflict skel + marker) được ghi vào WORK_QUEUE trong CÙNG transaction với việc đổi base. P07 danh sách item MUST không có item cài kết quả merge từ tmp gắn với base mới.
- **Failure scenario:** kill -9 sau khi NODES base = r101, trước khi diff3 chạy. Mở lại: base mới, working file vẫn là nội dung local cũ (dựa r100). Status báo "M" với diff = local edit + đảo ngược thay đổi server. User commit → âm thầm revert commit của người khác trên server. Không có conflict, không có `.mine`. Property test P11:59 chỉ kiểm khi không crash.
- **Evidence:**
  - `docs/system-design/02-architecture-data-flow-and-reliability.md:88` "─► cập nhật NODES base ─► chạy work queue (rename atomic, set flags, translate eol/keywords)"
  - `docs/system-design/02-architecture-data-flow-and-reliability.md:89` "─► file có sửa local: diff3 → merge sạch hoặc conflict skel + file .mine/.rN"
  - `phase-11-svn-client-working-copy-operations.md:21` "có sửa local → `merge3` (P05) → sạch hoặc conflict text (marker + conflict skel)" — không nói tới work item/txn.
  - `phase-07-svn-wc-database-and-pristine-store.md:28` danh sách MUST item, không có ràng buộc merge-result-install cùng txn với base update.
- **Suggested fix:** Ghi invariant vào P11 + P07: với mỗi file, merge3 chạy vào `.svn/tmp` TRƯỚC; trong 1 SQLite txn: cập nhật NODES base + conflict skel + WQ items (`file-install` từ tmp, `prej-install`/marker) — theo đúng `svn_wc__internal_merge` + `wq_build_file_install(source=tmp)`. Sửa thứ tự trong doc 02 §2.3. Thêm crash scenario "kill sau txn base trước WQ" vào P13 với file có sửa local.

## Finding 2: WC_LOCK mồ côi sau crash không có PID → luồng "mở lại → work queue tự hoàn tất" không thể xảy ra
- **Severity:** Critical
- **Location:** Phase 7 WC_LOCK; Phase 11 cleanup; Phase 13 Crash suite; docs 02 §3 bảng lỗi
- **Flaw:** WC_LOCK là row trong wc.db, không PID/hostname. Sau kill -9, row còn nguyên → lần mở sau engine thấy lock và phải trả `WcBusy` (không phân biệt được tiến trình chết của chính mình với TortoiseSVN đang chạy). Doc 02 lại hứa "tự chạy work queue nếu không ai giữ lock" — sau crash LUÔN có người "giữ". P11 cleanup "gỡ WC_LOCK của chính app" — sau restart không có cách xác định row nào "của app". P13 kỳ vọng mở lại → work queue tự hoàn tất.
- **Failure scenario:** (a) Crash giữa update → user mở app → banner "WC đang dùng bởi chương trình khác" vĩnh viễn; Retry vô ích. (b) Nếu implementer "dọn orphan" bằng heuristic → xóa lock thật của TortoiseSVN đang commit → hai writer đồng thời → hỏng wc.db (đúng rủi ro report §6 cấm).
- **Evidence:**
  - `reports/from-researcher-to-planner-libsvn-wc-db-format-report.md:123` "**No PID/hostname stored** -> stale locks only cleared by `svn cleanup` or steal."
  - `reports/from-researcher-to-planner-libsvn-wc-db-format-report.md:170` "Stale WC_LOCK rows: no PID -> need `svn cleanup`-equivalent; do not auto-steal silently."
  - `phase-07-svn-wc-database-and-pristine-store.md:29` "steal chỉ khi người dùng đồng ý; dọn orphan."
  - `phase-11-svn-client-working-copy-operations.md:26` "cleanup: chạy work queue, gỡ WC_LOCK của chính app"
  - `phase-11-svn-client-working-copy-operations.md:30` "kill giữa chừng → mở lại work queue hoàn tất."
  - `docs/system-design/02-architecture-data-flow-and-reliability.md:135` "Tự chạy work queue nếu không ai giữ lock"
- **Suggested fix:** Định nghĩa rõ: (1) app ghi sidecar ngoài wc.db (vd `app.sqlite` bảng `held_wc_locks(wc_root, dir, pid, boot_id, started)`) trước khi INSERT WC_LOCK; (2) khi mở WC, row WC_LOCK khớp sidecar có PID chết → coi là của mình, tự cleanup; (3) row không khớp → `WcBusy` + nút "Cleanup (break locks)" có xác nhận. Sửa P13 success criteria để kiểm cả hai nhánh. Ownership: P07 API `wc_lock_break`, P08 sidecar.

## Finding 3: Crash/cancel giữa MERGE và finalize wc.db, và txn rò — không phase nào sở hữu, không contract
- **Severity:** High
- **Location:** Phase 10 commit + Risk; Phase 11 commit; Phase 8 (thiếu); docs 02 §2.3 Commit
- **Flaw:** (a) Doc 02 cho rằng crash sau MERGE → "update giải quyết". Sai với file **add**/copy: server đã có path, WC vẫn schedule-add → update gặp "local add vs incoming add" = tree conflict/obstruction (P11 chỉ hỗ trợ 4 tree conflict cơ bản, không gồm case này). Lock đã release trên server nhưng token vẫn trong WC. (b) P10 đẩy việc ghi tên txn đang mở sang "file tạm của app (P08)" nhưng P08 không có requirement nào, `RaSession` trait không có hook/callback báo txn name, và crash harness P13 chạy CLI không có P08.
- **Failure scenario:** User commit 20 file mới; app bị kill ngay sau MERGE 200 OK. Mở lại: 20 file vẫn "A". User bấm Commit lại → server trả "already exists" → kẹt; bấm Update → 20 tree conflict mà MVP báo "dùng TortoiseSVN". Riêng kill trước MERGE: txn rò trên VisualSVN tích tụ (`db/transactions/`) vì không ai dọn.
- **Evidence:**
  - `docs/system-design/02-architecture-data-flow-and-reliability.md:103` "Crash sau MERGE trước khi cập nhật WC → lần mở sau WC báo \"out-of-date\" ...; `update` giải quyết"
  - `phase-10-svn-ra-remote-operations.md:69` "ghi tên txn đang mở vào file tạm của app (P08) để dọn sau."
  - `phase-08-app-backend-core-services.md:18-29` (Requirements) — không có mục txn tracking.
  - `phase-11-svn-client-working-copy-operations.md:60` "Commit lỗi giữa chừng → WC không đổi, server không có revision mới." — không có criterion cho crash SAU MERGE.
- **Suggested fix:** Thêm vào contract `RaSession::commit` một callback `on_txn_created(txn_name)` / `on_merged(new_rev)`; P11 ghi "commit journal" (ngoài wc.db hoặc WQ item `file-commit` chuẩn bị trước MERGE như libsvn `svn_wc__process_committed_queue`) để sau crash có thể finalize bằng new_rev mà không cần update. Gán owner dọn txn rò (P11 hoặc P08) + test P13 "kill sau MERGE" có kỳ vọng cụ thể.

## Finding 4: Crash suite chỉ giết tại ranh giới work item → 1000 vòng xanh mà không chứng minh gì
- **Severity:** High
- **Location:** Phase 13 Requirements + Implementation step 2; Acceptance criteria MVP
- **Flaw:** Yêu cầu nói "điểm ngẫu nhiên" nhưng cơ chế duy nhất được lên kế hoạch là `OPENSVN_TEST_KILL_AFTER_WQ_ITEMS=N` — đúng ngay các điểm an toàn. Không kill trong: stream REPORT/GET, giữa SQLite txn, giữa rename pristine, giữa PUT và MERGE, sau MERGE trước finalize, giữa merge3 và install (Finding 1). Harness là CLI riêng, không qua WcActor/AuthBroker/StatusService của P08 nên crash của app thật (watcher đang chạy status khi kill) không được thử.
- **Failure scenario:** Nightly báo "1000/1000, 0 hỏng" → MVP acceptance được tick; production crash ở giữa `rename` pristine hoặc post-MERGE (Finding 2/3) chưa bao giờ được exercise.
- **Evidence:**
  - `phase-13-integration-crash-safety-and-performance.md:19` "kill -9 tiến trình engine ... tại điểm ngẫu nhiên trong checkout/update/commit/revert/switch"
  - `phase-13-integration-crash-safety-and-performance.md:40` "điểm kill ngẫu nhiên (env `OPENSVN_TEST_KILL_AFTER_WQ_ITEMS=N` do P07 hỗ trợ"
  - `plan.md:111` "Crash test 1000 vòng kill ngẫu nhiên: 0 hỏng WC, 0 mất thay đổi local."
- **Suggested fix:** Kill bằng thời gian ngẫu nhiên từ bên ngoài (SIGKILL sau t ~ U(0, T_op)) + failpoint đặt tên (crate `fail`) tại: trước/sau commit SQLite txn, trước/sau rename, sau MERGE response, giữa GET skelta. Báo cáo coverage theo failpoint. Thêm biến thể chạy qua app thật (P08) với watcher bật.

## Finding 5: Luồng auth prompt giữa operation: replay body stream không thể, giữ WC_LOCK 5 phút, prompt trùng, contract không tồn tại cho P08
- **Severity:** High
- **Location:** Phase 4 Requirements (transport/auth); Phase 8 AuthBroker + step 6; Phase 1 contract; docs 03 §5
- **Flaw:**
  1. P04 body là stream không buffer, nhưng 401 → prompt → **retry** cùng request. PUT svndiff/POST/MERGE đã tiêu thụ body không replay được; đồng thời P04 cấm retry POST/PUT/MERGE → mâu thuẫn trực tiếp cho 401 giữa commit (VisualSVN với SSPI có thể 401 trên từng kết nối mới của pool).
  2. Prompt "tạm dừng operation" trong WcActor write → giữ WC_LOCK + chặn hàng đợi ghi tới 5 phút timeout; TortoiseSVN bị `WC locked` suốt thời gian đó.
  3. 4 GET song song (pool) cùng nhận 401 → không có single-flight theo realm → 4 dialog/4 lần đếm "tối đa 3 lần" → khóa tài khoản AD sau vài lần nhập sai. `RaSessionPool` dùng chung giữa nhiều WC (doc 02) không có owner trong ownership table.
  4. `AuthPrompter` không có trong contract P01 (chỉ ở `svn-ra` của P04) nhưng P08 nói dùng "trait tạm theo contract P01" → P08 tự chế trait, drift khi P04 merge.
- **Failure scenario:** Mật khẩu AD đổi; user commit 50 file. PUT thứ 3 nhận 401, body đã gửi một phần → không thể resend → commit fail giữa txn (DELETE txn best-effort), hoặc implementer buffer cả file 2GB vào RAM để replay → vỡ ngưỡng RAM. Cùng lúc 4 prompt bật, user nhập sai 3 lần ở mỗi cái → AD lockout.
- **Evidence:**
  - `phase-04-transport-auth-and-secure-store.md:19` "body stream (không buffer cả body) cả chiều gửi lẫn nhận"
  - `phase-04-transport-auth-and-secure-store.md:22` "401 → `AuthPrompter::credentials(realm)` tối đa 3 lần."
  - `phase-04-transport-auth-and-secure-store.md:23` "Retry tự động 3 lần ... chỉ cho method idempotent"
  - `phase-08-app-backend-core-services.md:23` "chờ `auth_respond` (timeout 5 phút → hủy)"
  - `phase-08-app-backend-core-services.md:58` "trước đó trait tạm theo contract P01" — `phase-01-...md:40-122` không định nghĩa `AuthPrompter`/`Credentials`/`CertInfo`.
  - `docs/system-design/03-ipc-api-contract-and-data-model.md:185` "engine gặp 401 → phát `auth://prompt`, **tạm dừng** operation"
  - `docs/system-design/02-architecture-data-flow-and-reliability.md:69` "RaSessionPool theo (repo root URL, user) ... dùng chung cho mọi WC" — không có trong `plan.md:70-89`.
- **Suggested fix:** (a) Body = `Fn() -> Stream` factory (rewindable từ file tạm/pristine) cho mọi request; 401 replay được phép vì server chưa xử lý body. (b) Pre-auth: gọi OPTIONS có xác thực trước khi lấy WC_LOCK/tạo txn; prompt không bao giờ xảy ra khi đang giữ WC_LOCK (fail → nhả lock → hỏi → chạy lại). (c) Single-flight prompt theo realm trong `AuthState`, bộ đếm attempt chung. (d) Đưa `AuthPrompter` + `Credentials`/`CertInfo` vào `svn-types` (P01), gán owner `RaSessionPool` (P10 hoặc P08).

## Finding 6: Hứa "hủy < 1s" nhưng điểm hủy chỉ giữa work item
- **Severity:** High
- **Location:** Phase 8 Success Criteria; Phase 11 Requirements; docs 02 §2.2
- **Flaw:** Cancel chỉ an toàn giữa các work-queue item, nhưng phần lớn thời gian checkout/update là stream REPORT + GET (chưa có work item nào) và một item `file-install` 2GB (translate eol) dài hàng chục giây. Không định nghĩa cancel trong pha mạng: `abort_edit`, trạng thái `incomplete` của NODES, txn còn mở (commit). P08 chỉ chứng minh < 1s trên FakeEngine.
- **Failure scenario:** User hủy checkout 50GB ở 45% → nút Cancel không phản hồi tới khi item hiện tại xong, hoặc implementer hủy ngay giữa luồng mạng mà không đánh dấu dir `incomplete` → WC nửa vời, `svn status` báo sạch nhưng thiếu file; update sau không tải lại vì base rev đã ghi.
- **Evidence:**
  - `phase-08-app-backend-core-services.md:65` "Hủy tác vụ dừng trong < 1s"
  - `phase-11-svn-client-working-copy-operations.md:29` "cancel ở điểm an toàn (giữa các work item)."
  - `docs/system-design/02-architecture-data-flow-and-reliability.md:70` "điểm hủy an toàn chỉ giữa các work-queue item"
  - `phase-11-svn-client-working-copy-operations.md:20` checkout "→ `do_update` ... → work queue → xong" — không nhắc presence `incomplete`/resume.
- **Suggested fix:** Định nghĩa 2 loại điểm hủy: (1) trong pha mạng — hủy ngay, `abort_edit`, dir đang mở giữ presence `incomplete` như libsvn để update sau tiếp tục; (2) trong pha WQ — giữa item, item lớn có checkpoint. Thêm test cancel < 1s với RealEngine + checkout lớn ở P13; thêm kịch bản "resume checkout bị hủy" vào compat.

## Finding 7: Hand-off RealEngine (P08 step 9) mồ côi: owner xong việc từ wave 1, không contract `EngineFacade` ↔ `SvnClient`
- **Severity:** High
- **Location:** Phase 8 Engine adapter + step 9; Phase 11 Related files + step 9; plan.md Waves
- **Flaw:** P08 ước lượng 3–4 tuần ở wave 1, nhưng bước nối RealEngine phụ thuộc P11 (xong ~tuần 18–22). Wave 2/3 không phân bổ agent cho P08; P11 bị cấm sửa `real.rs`. `EngineFacade` là trait nội bộ do P08 tự thiết kế dựa FakeEngine, còn API `SvnClient` do P11 track U tự thiết kế — không ai khóa contract giữa hai bên. P12 wave-3 và P13 phụ thuộc đúng mắt xích này.
- **Failure scenario:** Tuần 20 P11 merge; agent P08 đã kết thúc; lead phải gán ai đó học lại `src-tauri`. FakeEngine trả status đồng bộ, `SvnClient` cần `&mut WcDb` + WC_LOCK + progress callback khác shape → viết lại WcActor/StatusService; E2E P12 và crash/perf P13 trượt vài tuần — trên đường găng nhưng không nằm trong đường găng của plan.
- **Evidence:**
  - `phase-08-app-backend-core-services.md:6` "effort: \"3–4 tuần · 1 agent\""
  - `phase-08-app-backend-core-services.md:29` "trait nội bộ `EngineFacade` ... `RealEngine` (nối `svn-client` + `svn-ra` khi sẵn sàng"
  - `phase-08-app-backend-core-services.md:61` "RealEngine (wave 3): nối dần command remote (P10) rồi WC (P11)"
  - `phase-11-svn-client-working-copy-operations.md:43` "**không** (P08 sở hữu; P11 chỉ cung cấp API, P08 nối)."
  - `plan.md:62-64` wave 2 chỉ "agent wave 1 còn chạy: P07, P08"; wave 3 không có P08.
  - `plan.md:66` đường găng "P01 → P07 → P11 → P13" — bỏ qua P08-RealEngine và P12.
- **Suggested fix:** Tách "P08b RealEngine integration" thành gói riêng ở wave 3 có agent + ước lượng (2–3 tuần), đưa vào đường găng. Khóa `SvnClient` facade signature (async/sync, progress, cancel, lock semantics) như contract PR sớm (cuối wave 1), FakeEngine implement đúng trait đó.

## Finding 8: Quy tắc giải xung đột "giữ cả hai dòng" phá build; lockfile và `bindings.ts` không có/hai owner
- **Severity:** High
- **Location:** plan.md File ownership + Quy tắc 8; Phase 8 Related files; Phase 9 owner_paths
- **Flaw:** (a) 9 agent cùng thêm vào `[workspace.dependencies]`; nhiều phase cần cùng crate (`sha1`, `md-5` cho P02 và P07; `quick-xml`; `tokio` features khác nhau). "Giữ cả hai dòng" → khóa TOML trùng → `Cargo.toml` không parse → `main` đỏ cho mọi người. (b) `Cargo.lock` và `pnpm-lock.yaml` không có trong bảng ownership — mọi PR đều sửa → xung đột liên tục. (c) `apps/opensvn/src/ipc/bindings.ts` nằm trong path P09 nhưng sinh từ `opensvn-ipc`/command của P08 và P08 được ghi qua `pnpm gen:ipc` → 2 owner; mỗi contract PR làm CI "bindings lệch" fail trên mọi branch chưa rebase. (d) P12 không sở hữu `package.json` → không thêm được dependency FE.
- **Failure scenario:** P02 và P07 cùng thêm `sha1 = "0.10"` vs `sha1 = { version = "0.10", features = ["asm"] }`; người rebase "giữ cả hai" → `duplicate key` → toàn bộ CI 3 OS đỏ; 9 agent bị chặn tới khi lead sửa.
- **Evidence:**
  - `plan.md:72` "Phase khác chỉ được **thêm** dòng vào `[workspace.dependencies]`"
  - `plan.md:99` "xung đột trong `[workspace.dependencies]` giải quyết bằng giữ cả hai dòng."
  - `plan.md:84` P09 sở hữu `apps/opensvn/src/{app,components,ipc,styles,lib}/`
  - `phase-08-app-backend-core-services.md:49` "`apps/opensvn/src/ipc/bindings.ts` chỉ qua `pnpm gen:ipc`"
  - `phase-02-svn-delta-svndiff-codec.md:23` "`md-5`, `sha1`"; `phase-07-...md:27` pristine cần sha1/md5.
- **Suggested fix:** P01 khai báo trước toàn bộ dependency dự kiến (version + features) trong `[workspace.dependencies]`; thêm mới qua contract PR của lead. Quy tắc xung đột: hợp nhất thành 1 khóa, sắp xếp alphabet, CI kiểm `cargo metadata`. `Cargo.lock`/`pnpm-lock.yaml`: luôn regenerate sau rebase (không merge tay), ghi rõ. `bindings.ts`: owner = generator (P01/lead), CI tự regenerate, không phase nào commit tay.

## Finding 9: Mô hình "đọc song song với snapshot WAL" dựa trên giả định journal mode chưa xác minh
- **Severity:** High
- **Location:** docs 02 §2.2; Phase 8 WcActor; Phase 7 Non-functional + Bước 0
- **Flaw:** Thiết kế đồng thời P08 (đọc song song khi đang ghi) giả định WAL. Research chưa xác minh libsvn_wc dùng journal mode/busy timeout nào, và cảnh báo sai mode có thể hỏng truy cập đồng thời với TortoiseSVN. P08 chạy wave 1 song song với Bước 0 của P07 → P08 build WcActor/StatusService trên giả định WAL trước khi kết quả có. Nếu svn dùng rollback journal (hoặc `locking_mode=EXCLUSIVE`), app bật WAL sẽ đổi header file wc.db vĩnh viễn và để lại `-wal/-shm`; nếu giữ rollback journal thì status (watcher 300ms) đụng `SQLITE_BUSY` với chính update đang ghi.
- **Failure scenario:** App bật WAL trên wc.db; TortoiseSVN (SQLite bundled khác) mở WC trên share mạng/SMB (WAL không hỗ trợ trên network FS) → lỗi "database is locked"/hỏng; hoặc status nền trả `SQLITE_BUSY` liên tục trong update dài → banner lỗi giả.
- **Evidence:**
  - `docs/system-design/02-architecture-data-flow-and-reliability.md:67` "lệnh đọc (status, diff, log) chạy song song với snapshot đọc SQLite (WAL)."
  - `reports/from-researcher-to-planner-libsvn-wc-db-format-report.md:125` "Busy timeout value and journal mode UNVERIFIED"
  - `reports/from-researcher-to-planner-libsvn-wc-db-format-report.md:173` "wrong mode can corrupt concurrent access (UNVERIFIED which mode svn uses)."
  - `phase-07-svn-wc-database-and-pristine-store.md:34` "journal mode / busy timeout **giống svn** (xác minh ở bước 0)"
  - `phase-08-app-backend-core-services.md:20` "lệnh ghi tuần tự, lệnh đọc song song"
- **Suggested fix:** Đưa journal mode/busy timeout thành gate: kết quả Bước 0 P07 → contract PR cập nhật doc 02 trước khi P08 code WcActor read path. Nếu không phải WAL: đọc trong lúc ghi phải dùng snapshot status từ cache (không mở wc.db) hoặc chờ; thêm test đồng thời với svn CLI trên rollback journal; cấm đổi journal mode của wc.db.

## Unresolved questions
- Lead có chấp nhận thêm sidecar lock-ownership ngoài wc.db (Finding 2) không — ADR-004 nói "không thêm bảng" chỉ áp cho wc.db?
- Ai sở hữu `RaSessionPool` và dọn txn rò (P08, P10 hay P11)?
- Có tách P08b RealEngine thành gói wave 3 riêng không (ảnh hưởng ước lượng 22–26 tuần)?

Status: DONE
