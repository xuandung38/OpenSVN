# PR #44 review: P08 app backend M1 (from code-reviewer to lead)

- PR: xuandung38/OpenSVN#44 · Reviewed SHA `cf1551ca` (head đã lên `a1ae1879` trong lúc review, chỉ là commit doc 03 trusted_cert port; không ảnh hưởng findings)
- Scope: 69 file, +11202/-77. Worktree `<local>/OpenSVN-worktrees/review-pr44`, build dir trong scratch (đã xóa).

## Verdict

- **Commit app (`2eb26900`, `fd33d3c2`, merges): REQUEST CHANGES**. Có 1 gate fail (biome) và 5 lỗi IMPORTANT.
- **Commit contract `45e79c51` (opensvn-ipc): APPROVE kèm điều kiện**. Đúng, tối thiểu, thêm vào cuối nên không phá wire, bindings không drift. Điều kiện: cập nhật doc 03 (C1) trong PR này hoặc ngay sau đó.

## Gates (chạy local, CARGO_BUILD_JOBS=2)

| Gate | Kết quả |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy -p opensvn --all-targets --all-features -D warnings` | pass |
| `cargo clippy --workspace --all-targets -D warnings` | pass (chỉ macOS; nhánh cfg Windows chưa được kiểm, xem Q1) |
| `cargo test -p opensvn --all-features` | 103 passed |
| `cargo test -p opensvn-ipc` | pass (serde_shape 12) |
| `cargo test --workspace` | pass |
| `cargo deny check` | advisories/bans/licenses/sources ok |
| `pnpm gen:ipc` → so với `bindings.ts` đã commit | không drift |
| `pnpm typecheck` / `pnpm test` (vitest) | pass / 1 passed |
| **`pnpm lint` (biome)** | **FAIL**: `capabilities/default.json` sai format (`"windows": [\n "main"\n ]`) |
| unwrap/expect/unsafe trong code app | không có (chỉ trong test, có `#[allow]`); workspace `unsafe_code = forbid` |

## Đã kiểm, đạt

- Capability: chỉ cửa sổ `main`, không có `remote`/`webviews`; allowlist khớp `COMMAND_NAMES` + 2 core event perm + opener. Không có plugin fs/shell/dialog (có test kiểm Cargo.toml).
- Opener: chỉ `opener:allow-open-url` với `{"url":"https://*"}`, không có `opener:default`. Đã đọc source tauri-plugin-opener 2.7.1: `Application::Default` bắt `with == None`, nên JS không chọn được app để mở; glob khớp trên chuỗi thô nên `HTTPS://`, `file:`, `javascript:` đều bị chặn. `open_path`/`reveal_item_in_dir` không được cấp.
- CSP prod không có `unsafe-inline`/`unsafe-eval`/`*`; `blob:` trong img-src đúng theo plan. devCsp chỉ thêm `'unsafe-inline'` cho style và dev server.
- `settings_set` refuse các program key: patch được deserialize vào `SettingsPatch` có kiểu rồi mới serialize lại, nên key sai hoa thường, có khoảng trắng hay lồng nhau (`{"diff":{"externalTool":…}}`) đều bị serde bỏ, không bao giờ tới DB. Null (clear) cũng bị refuse. Có test IPC end-to-end.
- `program_choose`: path không bao giờ đến từ webview (signature `(kind, args)`), file picker và confirm đều native (rfd), lưu path canonical, `try_lock` không cho chồng dialog.
- Panic containment: `guard()` ở mọi command, `catch_unwind` trong `OperationManager::spawn` và trong job read/write của actor; có test actor vẫn chạy sau panic.
- WcActor: ghi tuần tự qua mpsc + `RwLock` write; đọc qua `try_read` với backoff 25→400ms, tổng 8 lần (~1,6s) rồi trả `wc-busy` retryable.
- AppDb: WAL chỉ bật trên `app.sqlite`/`cache.sqlite`; không có chỗ nào mở `.svn/wc.db` (grep). Migration dùng `user_version`, mỗi bước một transaction, từ chối DB của bản mới hơn. `trusted_cert` PK `(host, port, sha256)`, `account` PK `(scheme, host, port, realm)`, có test.
- Contract: `OperationKind` thêm ở cuối, `ProgramKind` kebab-case, 3 nullable key mới có test 3 trạng thái.

## Findings

### [IMPORTANT] I1. Biome gate fail
`apps/opensvn/src-tauri/capabilities/default.json:5-7`. CI sẽ đỏ.
Fix: `pnpm exec biome format --write apps/opensvn/src-tauri/capabilities/default.json` (thành `"windows": ["main"]`).

### [IMPORTANT] I2. Asset scope `$TEMP/opensvn/**` thoát được trên Linux nhiều user
`tauri.conf.json:26-28`. `$TEMP` = `std::env::temp_dir()`, trên Linux là `/tmp` dùng chung cho mọi user. Đã đọc tauri 2.12.3 `scope/fs.rs::push_pattern`: ngoài pattern gốc, Tauri còn **thêm bản canonicalize của pattern** (`canonicalize_parent`). Nếu một user khác tạo trước `/tmp/opensvn -> /home/victim` thì lúc app khởi động scope sẽ chứa `/home/victim/**`. Thêm nữa, file tạm image-diff sau này sẽ được ghi vào thư mục do user khác sở hữu (đọc trộm nội dung repo private, tấn công symlink khi ghi). macOS (`$TMPDIR` riêng từng user) và Windows (`%TEMP%` riêng từng user) không bị.
Fix: bỏ scope tĩnh. Lúc runtime tạo thư mục riêng user, quyền 0700 (ví dụ `app.path().app_cache_dir()/preview`, hoặc thư mục tên ngẫu nhiên tạo bằng `create_dir` + kiểm owner), rồi gọi `app.asset_protocol_scope().allow_directory(dir, true)`. Hoặc tối thiểu dùng `$APPCACHE/preview/**`. Thêm test: scope không được mở rộng khi `opensvn` là symlink.

### [IMPORTANT] I3. Hủy op đang chờ trong hàng đợi không xong trong < 1s
`app_state.rs:113-135`, `wc/actor.rs:92-116`. `start_wc_op` chuyển `ctx` vào job rồi đẩy vào mpsc của actor. Nếu job xếp sau một update dài thì `op_cancel` chỉ bật token, và op vẫn ở trạng thái `running` tới khi job trước chạy xong (có thể vài phút). Vi phạm success criterion "Hủy < 1s". Test hiện có chỉ hủy op đang chạy.
Fix: trong closure của `ops.spawn`, race `actor.write(...)` với `ctx.cancel.cancelled()` (`CancelToken::cancelled()` có sẵn), đồng thời trong job wrapper kiểm `ctx.cancel.is_cancelled()` trước khi lấy gate thì bỏ qua. Thêm test: write A dài, write B đang chờ, hủy B thì nhận `op://failed{cancelled}` trong < 1s.

### [IMPORTANT] I4. Liveness probe: pid tái sử dụng và lệch boot time
`lock_ownership/alive_probe.rs:13,34-50,66-71`, `lock_ownership/mod.rs:62-70`.
- Trong cùng một boot, `pid alive` chỉ dựa vào pid. Pid của app đã crash có thể bị process khác dùng lại (Windows tái dùng rất nhanh, macOS/Linux sau uptime dài). Khi đó lock cũ bị coi là "alive" mãi tới lần reboot: WC kẹt `wc-busy`/needs-cleanup mà cleanup không tự gỡ.
- Ở chiều nguy hiểm hơn: trên macOS/Windows boot id là `boot-time:<now - uptime>`, giá trị này dịch theo đồng hồ hệ thống. Nếu đồng hồ nhảy > 120s (chỉnh tay, NTP sửa lớn sau sleep) thì một instance khởi động sau sẽ coi lock của instance **còn sống** là chết và chiếm lấy, có nguy cơ hỏng wc.db. App chưa có cơ chế single-instance.
Fix: (a) lưu thêm start time của process (sysinfo `Process::start_time()`) khi `record`; `is_alive` chỉ true khi pid sống **và** start time khớp (có dung sai). Cột có thể thêm thẳng vào migration v1 vì chưa release. (b) Khi boot-time không khớp nhưng pid đang sống với start time khớp thì coi là "unknown" (needs user action), không phải "dead". (c) macOS: ưu tiên `kern.bootsessionuuid` (sysctl, không cần unsafe trong code mình). Store chưa được nối vào đâu (P15), nên sửa bây giờ rẻ.

### [IMPORTANT] I5. Webview đọc và ghi file tùy ý trên host qua lệnh repo (FakeEngine đang là engine mặc định của bản build)
`engine/fake/remote_ops.rs:194` (`repo_save` ghi vào `save_to` tùy ý), `:602-614` (`RemoteOp::Upload` đọc `local_path` tùy ý), `lib.rs:40-44` (FakeEngine luôn bật trong `run()`). Chuỗi tấn công: `remote_edit{Upload local_path=~/.ssh/id_rsa}` rồi `repo_cat` trả nội dung về webview, hoặc `repo_save` copy file sang đường dẫn bất kỳ. Cách này phá đúng threat model "WebView có thể bị chiếm" mà PR áp dụng cho program settings. Với RealEngine (P15) còn nặng hơn: Upload tới URL do webview chọn là exfil ra server ngoài.
Fix (cần lead quyết, đây là vấn đề contract): đường dẫn cục bộ ngoài WC (`saveTo`, `localPath`, checkout `path`) phải đến từ dialog native phía Rust (theo mẫu `program_choose`, hoặc token do dialog trả về), không nhận path thô từ webview. Trong PR này tối thiểu: FakeEngine không chạm FS thật (Upload/Save chạy trong bộ nhớ, hoặc chỉ khi bật feature `fake-fs` cho dev), và để FakeEngine/demo seed sau `cfg(debug_assertions)` hoặc một feature.

### [IMPORTANT] I6. Dialog xác nhận có thể bị giả mạo bằng args do webview gửi
`programs/mod.rs:75-84,116-120`. `args` lấy từ webview, chỉ chặn `\0` và giới hạn 32×1024. Webview chèn `\n`×N, ký tự bidi (U+202E) hoặc chuỗi dài để đẩy/che phần nguy hiểm khỏi `command_line` trong dialog (ví dụ user chọn VS Code thật, args thêm `--install-extension <vsix>`, hoặc chọn shell/terminal kèm `-c …`). Ghép bằng dấu cách cũng làm mơ hồ ranh giới các arg.
Fix: từ chối mọi `char::is_control` và các ký tự định dạng bidi/zero-width trong args; giới hạn tổng độ dài (khoảng 2 KB); hiển thị mỗi arg một dòng, có quote. Nếu được thì chỉ cho args chứa các placeholder đã biết (`%base %mine %theirs %merged %l %r`) cùng flag tĩnh.

### [MODERATE] M1. Publish status bị race, cache có thể quay về trạng thái cũ
`status/publisher.rs:89-110`. `publish()` đọc dưới read lock dùng chung, nên 2 lần publish (refresh + op vừa xong) có thể chạy song song. Chuỗi `apply_local` (đọc snapshot, tính delta, replace) không nguyên tử, nên kết quả cũ có thể ghi đè kết quả mới và phát delta "lùi".
Fix: thêm `Mutex` (hoặc counter thế hệ) theo từng WC bao quanh `apply_local`/`publish_revision`; bỏ kết quả có thế hệ thấp hơn bản đã áp.

### [MODERATE] M2. Khởi động chặn main thread
`lib.rs:59-65`. `block_on(recover_all())` chạy `open_wc` tuần tự cho mọi WC trong `setup`, trước khi cửa sổ hiện. Với FakeEngine thì nhanh, nhưng với RealEngine (recovery, work queue) thì cửa sổ sẽ trễ, trái NFR "không chặn main thread".
Fix: `tauri::async_runtime::spawn` sau khi `manage(state)`, trả kết quả qua banner/event.

### [MODERATE] M3. Demo WC được ghi vào app.sqlite thật của user
`lib.rs:61-64`. Lần chạy đầu ghi `/opensvn-fake/demo` vào DB lâu dài. Sau P15, row này sẽ cảnh báo "recovery failed" ở mỗi lần khởi động và nằm trong danh sách.
Fix: chỉ seed khi `cfg(debug_assertions)`/feature, hoặc đánh dấu row là demo và xóa khi đổi engine.

### [MODERATE] M4. Bộ conformance chưa đủ để bàn giao cho P15
`engine/conformance.rs`. 13/37 method của facade không có scenario nào: `open_wc` (recovery lock, rủi ro cao nhất của P15), `switch`, `cleanup`, `move_path`, `set_depth`, `upgrade_format`, `props_set`, `changelist_set`, `ignore_add`, `mark_move`, `repo_save`, `diff_rev`, `blame`. Hủy chỉ được test với token đã hủy sẵn, chưa có hủy giữa chừng.
Fix: thêm scenario cho các method trên, và hook `Harness::plant_stale_lock(dead|foreign)` để chạy scenario recovery giống nhau trên cả 2 engine.

### [MODERATE] M5. Migration bị race khi 2 instance cùng khởi động lần đầu
`db/migrations.rs:32,41`. `user_version` được đọc ngoài transaction, transaction lại là DEFERRED. Instance thứ hai sẽ gặp `table already exists` và app không khởi động được.
Fix: `transaction_with_behavior(Immediate)` rồi đọc lại `user_version` bên trong, bỏ qua bước đã áp.

### [MODERATE] M6. `root_key` chỉ chuẩn hóa từ vựng
`lock_ownership/mod.rs:28-34`. Symlink, khác hoa thường (APFS/NTFS) hoặc `..` cho ra key khác nhau cho cùng một WC, nên đường "dead owner" có thể bỏ sót row. Fix: canonicalize ở caller (hoặc trong store), ghi rõ trong doc của contract.

### [MODERATE] M7. Path canonical dạng verbatim trên Windows
`programs/mod.rs:40,72`. `canonicalize` trả `\\?\C:\…`; `.bat`/`.cmd` chạy qua cmd.exe không chạy được với path verbatim. Fix: `dunce::simplified`, hoặc tự bỏ tiền tố `\\?\` khi là VerbatimDisk.

### [MODERATE] M8. Sai error code
`settings/validate.rs:5`, `programs/mod.rs:33`. Lỗi validate và các lần refuse đều mang code `internal`, nên UI không phân biệt được lỗi nhập liệu với bug. Fix: dùng một code phù hợp (thêm `invalid-argument` vào contract, hoặc tạm map sang `permission-denied`/có `actions: open-settings`).

### [MODERATE] M9. Throttle chỉ lấy cạnh đầu, mất sample cuối
`ops/throttle.rs:31`. Sample cuối nằm trong cửa sổ 100ms bị bỏ, nên UI có thể đứng ở 97% tới `op://finished`. Fix: giữ sample bị bỏ và flush ở tick tiếp theo hoặc trước khi `finish`.

### [MODERATE] M10. Actor nhân đôi khi remove rồi add lại cùng root
`wc/registry.rs:193-206`. `remove` bỏ actor khỏi map trong khi op cũ vẫn giữ `Arc<WcActor>`. Nếu add lại cùng root thì id mới và actor mới, nên 2 writer chạy song song trên cùng wc.db. Fix: key actor theo `root_path`, hoặc từ chối remove khi `is_writing()`.

### Contract commit `45e79c51`

- [MODERATE] C1. Doc 03 chưa cập nhật: §2 `OperationKind` thiếu `status-refresh|add|delete|move|copy|save`; §3.5 thiếu `program_choose`/`program_clear`; §6.3 thiếu `diff.mergeTool`, `tools.editor`, `tools.terminal` và ghi chú "settings_set refuses". Doc 03 là contract, cần đồng bộ.
- [MODERATE] C2. `SettingsPatch` (TS) vẫn có các program key, nên FE type-check pass nhưng runtime luôn bị refuse. Nên bỏ 4 key này khỏi `SettingsPatch` (giữ trong `Settings`) để việc refuse diễn ra ngay lúc compile. Đổi tên `ExternalDiffTool` thành `ExternalProgram` thì ngữ nghĩa đúng hơn cho editor/terminal (không bắt buộc).

## Recommended actions (thứ tự)
1. I1 biome format (chặn CI).
2. I3 hủy op đang chờ, I2 asset scope theo từng user.
3. I6 lọc args; I5 FakeEngine không chạm FS và để sau cfg/feature, đồng thời lead quyết trust model của path (contract).
4. I4 thêm start time và hướng xử lý "unknown" vào liveness (trước P15).
5. M1, M2, M5, M10, rồi M4 trước khi P15 bắt đầu.
6. C1 cập nhật doc 03.

## Unresolved questions
- Q1: CI không chạy được (billing), nên clippy/test nhánh `cfg(windows)` (`validate_program`, chỗ dùng sysinfo trên Windows) chưa có ai kiểm. Có runner Windows nào khác không?
- Q2: Trust model cho path cục bộ do webview gửi (`saveTo`, `localPath`, checkout `path`): giữ nhận path thô như doc 03 hiện tại, hay chuyển sang dialog native/token như `program_choose`? Lead cần quyết (ảnh hưởng contract và P12).
- Q3: Bản build M1 có đóng gói cho người dùng không? Câu trả lời quyết định I5/M3 là blocker hay chỉ cần cấu hình dev.

Status: DONE
Summary: App commits REQUEST CHANGES: biome gate fail, asset scope `$TEMP` thoát được trên Linux, hủy op đang chờ quá chậm, liveness dễ hỏng khi pid tái dùng hoặc boot time lệch, webview đọc/ghi FS qua FakeEngine, args giả mạo được dialog xác nhận. Contract commit APPROVE với điều kiện cập nhật doc 03.

---

## Re-review at `c3c6e5c` (2026-10-13)

New commits: `f19faf2` (contract: PathGrant, `fs://dropped`, `invalid-argument`, program keys only in Settings), `51348e8` (fixes), `52fd5d0` + `4e36560` (contract: auth prompt reason/attempt/offered, Negotiate, CertFailureInfo), `2b1ea0d` (doc 03: auth + log paging), `c3c6e5c` (log paging tests), plus merges from main.

### Gates (local, CARGO_BUILD_JOBS=2)
fmt pass · clippy app (all-features) + workspace `-D warnings` pass · `cargo test -p opensvn` 127 passed · ipc + workspace tests pass · cargo deny ok · **biome pass** · tsc pass · vitest 61 passed.
`pnpm tauri build --debug --no-bundle` OK. I ran the binary with `OPENSVN_DATA_DIR=<scratch>`. The demo WC `/opensvn-fake/demo` was registered and `last_opened_at` was set (background recovery ran), all under `<data>/fake-engine/app.sqlite`. Nothing was written to `<data>/app.sqlite`, and `preview/` was created with mode 0700. The FE uses the real Tauri adapter inside the WebView (`ipc/client.ts`). **I did not check the UI visually.** A desktop screenshot captured another app, so I deleted it and took no more.

### Status of earlier findings
| # | Status | Evidence |
|---|---|---|
| I1 biome | FIXED | `pnpm lint` passes |
| I2 asset scope | FIXED | static scope is `[]`. `preview.rs` creates `<data>/preview` 0700 and refuses a symlink, group/other permissions, or no write access. Scope is granted at runtime with `allow_directory`. Tests cover the symlink and 0777 cases |
| I3 queued cancel | FIXED (residual race, see N2) | `write_cancellable` plus test `cancelling_a_queued_write_returns_at_once_and_the_job_never_runs` |
| I4 liveness | PARTIAL (see N1) | stores start_time. Boot id mismatch with a live process gives Unknown, which is never stolen (test). macOS uses `kern.bootsessionuuid` |
| I5 path trust | FIXED | PathGrant: single use, 5 min TTL, consumed on a wrong kind. `repo_save`, Upload, checkout `dest` and `wc_add_existing` all redeem a grant. `fs://dropped` is wired in `on_window_event` and only for window `main`. Tests in `commands_paths.rs` |
| I6 confirm dialog | FIXED (small gap, see N3) | `is_deceptive` rejects control chars, ZW, bidi and BOM. Total args ≤ 2048. Each arg is on its own line, quoted and escaped. The program path is checked too |
| M1 publish race | FIXED | generation ticket plus `applied` mutex. Test with 8 concurrent publishes |
| M2 block_on at startup | FIXED | `finish_startup` is spawned. The remaining `block_on(FakeEngine::demo)` only builds memory and is acceptable |
| M3 demo DB | FIXED | `fake-engine/` subdir (checked at runtime) |
| M4 conformance | FIXED | all 37 facade methods are called. `Harness::plant_lock` exists. `open_recovers_dead_locks_and_reports_foreign_ones` was added |
| M5 migration lock | FIXED | IMMEDIATE tx with `user_version` re-read inside it. 4-thread race test |
| M6 root_key | FIXED | canonicalize, and on Windows strip `\\?\` and lowercase |
| M7 `\\?\` | FIXED | `strip_verbatim` (drive paths only; UNC is left as is, OK) |
| M8 error code | FIXED | `ErrorCode::InvalidArgument` and `errors::invalid_argument` used across the app |
| M9 throttle final sample | FIXED | `pending_progress` is flushed in `finish`. Test `the_last_sample_is_not_lost_to_the_throttle` |
| M10 actor duplication | FIXED | actors keyed by `root_path` for the life of the process. Test `remove_and_re_add_reuse_the_same_actor` |
| C1 doc 03 | FIXED | doc covers OperationKind, program_*, path_pick, grants, auth and log paging |
| C2 SettingsPatch | FIXED | program keys exist in `Settings` only. `deny_unknown_fields` on the patch |

### New findings

**[IMPORTANT] N1. Linux: comparing start_time with a 2s tolerance can mark a live owner Dead after a clock step.**
`lock_ownership/mod.rs` (`START_TIME_TOLERANCE_SECS = 2`, `classify`). On Linux, sysinfo 0.39.6 computes `Process::start_time = start_ticks_since_boot/HZ + btime` (`unix/linux/process.rs:585`). `btime` comes from `/proc/stat` (`system.rs:116`), and the kernel moves it whenever the wall clock is stepped (NTP step, VM resume, manual change). If the clock steps more than 2s after `record`, the same live process gets a different start_time, `classify` returns Dead, and its lock is stolen. That is the dangerous direction (two writers on wc.db). Before this fix, Linux used pid plus the exact boot_id, so it did not have this problem.
Fix: store a start time that the clock cannot move. On Linux use seconds since boot: `p.start_time() - System::boot_time()`, both read in the same probe call, or field 22 of `/proc/<pid>/stat`. Exact boot_id already covers the boot. macOS and Windows keep the wall-clock creation time, which the kernel stores once and does not change. Also add a test with a fake probe that shifts boot time.
The store is not wired yet (P15), so this does not block merge. It **must** be fixed before P15 wires the store into svn-wc.

**[MODERATE] N2. Residual race in `write_cancellable`.** `wc/actor.rs:131-163`. The wrapper's `tokio::select!` is not `biased`. If cancel fires while the wrapper is waiting for the write lock and both branches become ready, it can take the gate and set `started` after the caller has already returned `cancelled`. The job then runs with a cancelled token: engines that check the token stop at once, but a method that ignores `ctx` (several FakeEngine local ops use `_ctx`) can still apply its write after the UI has shown "cancelled". Fix: `biased;` with the cancel branch first, re-check `is_cancelled()` after taking the gate, and use an `AtomicU8` CAS (`PENDING→STARTED` vs `PENDING→ABANDONED`) so the caller and the wrapper agree on one outcome.

**[MODERATE] N3. `is_deceptive` misses U+2028/U+2029 (line/paragraph separator, not `Cc`), U+061C (ALM), U+180E and U+00AD.** U+2028 renders as a line break in native dialogs and can push the rest of an arg out of view. Fix: add these to the `matches!`.

**[MODERATE] N4. The grant's window binding is nominal.** Mint and redeem both use the constant `MAIN_WINDOW`, so the check `g.window != window` can never fail. Real protection still comes from the capability (`windows: ["main"]`), so this is not a hole. Fix: redeem with `window.label()` taken from a `tauri::Window` param on the command, or correct the doc/contract text so it does not promise a window binding.

**[MODERATE] N5. `deny_unknown_fields` on `SettingsPatch`.** A patch with an unknown key fails during Tauri arg deserialization and returns a plain string error, not an `AppError` shape, so the FE `typedError` sees an unexpected shape. Low impact because the TS types prevent this. Optional fix: map it in the FE `unwrap`, or accept `serde_json::Value` and validate it into an `AppError`.

### Contract commits (`f19faf2`, `52fd5d0`, `4e36560`, docs `2b1ea0d`)
- PathGrant types are minimal and the real path never reaches the UI (`PathGrantInfo` only has a display name). `PickRequest` is tagged. `DroppedPaths` carries grants and position. Consistent with the app code.
- `AuthPromptReason`, `attempt`, `offered: AuthMethodKind[]`, `AuthResponse::Negotiate`, and `CertFailureInfo` with `Changed{oldSha256,newSha256}` match doc 03 and have serde tests. `ErrorCode::InvalidArgument` is additive (kebab wire).
- Log paging is documented: newest first, `fromRev` is an inclusive upper bound, `nextRev` is the first revision not yet returned, filter is applied before paging. `c3c6e5c` adds tests for these semantics.
- Breaking for the FE: `CheckoutRequest.path→dest`, `Upload.local_path→source`, `wc_add_existing(path→grant)`, `repo_save(saveTo→grant)`. The bindings were regenerated and tsc/vitest pass, so P09/P12 must call `path_pick` first.

### Re-review verdict
- **App commits: APPROVE**, on condition that N1 is fixed before P15 wires `SqliteLockOwnership` (track it as a P15 blocker). N2–N5 can be follow-ups.
- **Contract commits: APPROVE.**

### Unresolved
- Q1 is still open (no Windows runner, so `cfg(windows)` code is unchecked).
- The UI was not checked visually, only the backend state and the FE adapter selection. If the PO needs a screenshot of the window, someone has to run `pnpm tauri dev` on a screen that has no private data.

Status: DONE
Summary: Re-review at c3c6e5c: every earlier IMPORTANT and MODERATE item is fixed and all gates pass. The debug build ran and registered and opened the demo WC in fake-engine/app.sqlite. New finding N1 (Linux start_time moves with clock steps, so a live lock can be marked Dead) must be fixed before P15. APPROVE for both the app and contract commits.
