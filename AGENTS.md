# AGENTS.md: hướng dẫn và tiến độ cho người và agent cùng làm OpenSVN

Cập nhật lần cuối: 2026-10-16.
Đọc file này trước khi nhận việc. Lệnh build/test, quy tắc đặt tên nhánh và cấu trúc repo nằm ở `docs/contributing.md`. Kiến trúc nằm ở `docs/system-architecture.md`.


---

## 1. Quy tắc bắt buộc (chủ repo đặt, không ngoại lệ)

1. **Danh tính commit**
   - Mọi commit phải là `Hồ Xuân Dũng <me@hxd.vn>`. Đặt `git config user.name "Hồ Xuân Dũng"` và `git config user.email me@hxd.vn` trước khi commit.
   - Commit message không có trailer: không `Co-Authored-By`, không link phiên làm việc.
2. **Không ghi công công cụ AI**
   - Áp dụng cho commit, mô tả PR, comment, review và file trong repo.
   - Không có dòng kiểu "Generated with …", không tên công cụ hay hãng AI, không footer tự động.
   - Công cụ tự chèn footer thì phải xóa ngay, bằng lệnh sửa PR hoặc sửa comment.
3. **Không nêu tên công ty hay chủ quản** ở bất kỳ đâu trong repo.
4. **Không ghi hostname thật của công ty.** Dùng `*.test` hoặc `example.com`. Script `fixtures/check-no-real-hosts.sh` phải pass.
5. **Git**
   - Chỉ merge commit. Không rebase, không amend, không force-push nhánh của người khác.
   - Mỗi PR có một reviewer độc lập. Reviewer chạy lại gate rồi merge bằng merge method.
6. **Bảo mật**
   - TLS mặc định từ chối (`CertDecision::is_valid_for`), không bao giờ nới.
   - Đường dẫn ngoài working copy cần `PathGrant`.
   - Không bao giờ xóa dữ liệu người dùng khi tình huống mơ hồ.
   - Secret chỉ nằm trong keychain của OS, không bao giờ ghi vào log.
7. **Tương thích**
   - Working copy phải dùng chung được với svn 1.8–1.15 (wc.db format 31).
   - Server mục tiêu là VisualSVN (HTTPS, AD/Windows SSO).

## 2. Gate trước khi mở PR ready

GitHub Actions hiện **không có runner**: mọi job kết thúc sau 2–5 giây, không log, kể cả trên main. CI đỏ không phải do code. Gate thật là gate chạy trên máy. Ghi số lượng test vào mô tả PR.

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p opensvn --features real-engine --all-targets -- -D warnings
# failpoints, nếu chạm svn-wc/svn-client:
cargo clippy -p svn-wc -p svn-client --features failpoints --all-targets -- -D warnings
OPENSVN_REQUIRE_SVN=1 cargo test --workspace --locked      # oracle svn 1.15.0 (build bằng nix), đặt OPENSVN_SVN_BIN/OPENSVN_SVNADMIN_BIN
cargo test -p opensvn --features opensvn/real-engine
cargo test -p svn-wc --features failpoints && cargo test -p svn-client --features failpoints && cargo test -p opensvn --features failpoints
# FE (apps/opensvn):
pnpm install --frozen-lockfile && pnpm lint && pnpm typecheck && pnpm lint:safety && pnpm test && pnpm build
pnpm gen:ipc   # không được để lại diff
# Tùy chọn, khi chạm UI: axe WCAG 2.2 A/AA (có color-contrast) trên `dev:mock`, phím mũi tên trên slider,
# screenshot các trạng thái và /__gallery ở 2 theme, Chromium và WebKit (trình duyệt chưa cài thì bỏ qua và ghi chú)
pnpm test:browser   # ảnh ở apps/opensvn/test-results/browser
```

- **Chạm transport/RA:** chạy thêm DAV suite `real_engine_dav -- --ignored` với Apache + mod_dav_svn 1.14.3 và 1.15.0, chạy thẳng và qua `scripts/test-env/flaky-tls-proxy.py`.
- **Chạm wc.db/update/commit:** chạy thêm crash suite với 2 seed.
- **Thiếu dung lượng đĩa:** đặt `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`.

Build bộ cài:
- Linux: `scripts/release/build-linux.sh`.
- Windows (cross từ Linux): `pnpm tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc --bundles nsis --features release --config src-tauri/tauri.bundle.conf.json --config ../../packaging/generated/tauri.version.conf.json`.
- macOS: phải build trên máy Mac, dùng `scripts/release/build-macos.sh`.

---

## 3. Đã xong (trên main)

| Mảng | PR |
|---|---|
| Checkout/update nhanh ngang hoặc hơn svn CLI (0,77–0,9×) | #142 |
| Status nhanh hơn svn (100k file: 0,17 s, svn 0,26 s) | #139 |
| Commit gửi delta | #140 |
| An toàn khi crash, phục hồi work queue | #138 |
| Link web VisualSVN `…/!/#Repo` đổi thành URL repo, kể cả link trỏ revision cũ `r<N>` | #154, #163 |
| Settings mở được trên macOS, giới hạn ô nhập lấy chung một nguồn từ Rust | #155 |
| Bảng tiến độ thao tác, banner khóa đúng, kiểm tra nền chạy im lặng | #156 |
| Tự thử lại khi server cắt kết nối, tự tải tiếp checkout/update khi REPORT đứt | #157 |
| Đọc dạng stream (log/list/cat/blame/lock) tự thử lại hoặc tải tiếp; dừng sau 2 lần không tiến triển | #164 |
| Thao tác ghi không đi vào kết nối đã chết; không bao giờ gửi một thao tác ghi hai lần | #166 |
| Tree-conflict và thay file↔thư mục khớp svn 1.15 | #162, #165 |
| Obstruction trong status, WcBusy, symlink | #147, #152 |
| Fuzz hằng đêm (svn-protocol, svn-delta), proptest update so với svn 1.15, độ trễ watcher (~460 ms với 10k file), kiểm tra accessibility bằng axe (en/vi) | #168 |
| Golden giao thức từ mod_dav_svn 1.15.0 (`tests/data-1.15`, `fixtures/protocol-1.15`), test golden và replay chạy trên cả 1.14 và 1.15; snapshot `list_report_request`/`parent_listing_request`; fuzz 8 target svn-protocol mỗi target 1 giờ, không crash | #184 |
| `LogProgress::accept` chịu được entry merge lồng (`-g`): entry con không còn kéo lệch `last`, không bị bỏ hay lặp khi tải tiếp | #164 (follow-up) |
| Switch kiểm tra ancestry như svn 1.15: URL không chung lịch sử bị từ chối (E195012, `UnrelatedResources`) trước khi đổi gì; tùy chọn "Ignore ancestry" trong hộp thoại Switch; REPORT `get-location-segments` (`RaSession::get_location_segments`) và `svn_client::youngest_common_ancestor` | #186 |
| Mất mạng lâu khi checkout/update: chờ mạng (`network.reconnectWaitMin`, mặc định 10 phút) rồi tải tiếp đúng revision; "Tiếp tục checkout" sau khi hủy, đóng app hoặc crash (journal checkout); phát hiện ngủ/thức bằng đồng hồ boot, không theo giờ hệ thống | #172 |
| Thanh bên History hiện trạng thái đang tải, lỗi kèm nút Thử lại và danh sách trống, giống màn History chính | #174 |
| Phản hồi hỏng từ server (XML/UTF-8/base64/svndiff lỗi, editor drive sai) báo mã `server-protocol-error` (engine: `ErrorCode::ProtocolViolation`), không còn gợi ý Cleanup; `Corrupt` chỉ dành cho dữ liệu local | #175 |
| Browse báo số mục bị ẩn vì tên không an toàn, không còn hiện "thư mục trống" (chỉ gửi số đếm qua IPC) | #176 |
| Lock/unlock lỗi khi chưa mở được kết nối (connect timeout, reset lúc handshake) báo lỗi mạng thường, thử lại được; `WriteOutcomeUnknown` chỉ còn khi yêu cầu có thể đã tới server (`SvnError::not_sent`) | #191 |
| "Retry now" và đánh thức sau ngủ chỉ đánh thức thao tác đang chờ mạng, không để lại permit cho lần chờ sau | #191 |
| Test IPC qua mock runtime cho `add_paths`, `diff_external`, `status_refresh`, `menu_set_layout`, `op_retry_now`; proptest diagnostics preview→export quét byte zip (sửa lộ cookie thứ hai của header `Cookie` và giá trị cờ `--password=`) | #191 |
| Biến thể Samba AD DC của `visualsvn-like.sh` (`VSVN_KDC=samba`, AES-only, service account có SPN), `visualsvn_like` 18/18 và DAV suite 27/27 với mod_dav_svn 1.14.3 và 1.15.0; Secret Service round trip trên Linux headless; bộ nhớ 1 GiB tải lên/xuống (+3–4 MiB) chạy nightly | #191 |
| Switch tự tải tiếp khi REPORT/GET bị cắt (neo lại tại thư mục đích, `link_path` cho phần chưa tới, cuối drive ghi lại `repos_relpath` cả cây như svn 1.15); switch dừng hẳn để lại WC nhất quán, "Tiếp tục switch" (bảng `switch_journal`) hoàn tất bằng chính switch đó. Parser chấp nhận `delete-entry rev="-1"` (switch giữa hai thư mục không cùng lịch sử). MERGE của remote edit/copy mất kết quả: xóa txn có kiểm tra, không để txn lại trên server. Suite DAV qua flaky proxy có kỳ vọng tường minh (`OPENSVN_COMPAT_FAULTS`, `write_op!`) | #193 |
| Continue checkout do svn 1.15 bỏ dở: file svn đã đặt lên đĩa nhưng chưa kịp commit hàng BASE (bị kill giữa rename và commit) được nhận lại khi nội dung khớp, không còn tree-conflict và hàng `base-deleted`; file khác nội dung vẫn giữ nguyên làm vật cản | #172 (follow-up) |
| History của working copy không giữ working copy suốt lời gọi mạng: `log_get` chỉ tra vị trí node (`EngineFacade::node_location`) dưới khóa đọc, rồi đọc log qua mạng ngoài actor (`LogScope::Repo`). Thao tác ghi xếp hàng trong lúc log chậm không còn bị chặn; subtree đã switch vẫn đọc log theo URL của node | #192 (follow-up) |
| Engine và svn 1.14/1.15 dùng chung working copy qua HTTPS: `EngineDriver` cho compat harness, 12 scenario `client/op-*` xen kẽ engine với svn trên cùng WC, interop DAV hai chiều so wc.db/status/info; suite oracle svn-client chạy được với svn 1.14; failpoint `net.update.mid_stream`; `docs/tortoisesvn-checklist.md`. Sửa kèm: cache inherited props (`NODES.inherited_props`, REPORT `inherited-props-report`), đích move lên revision mới khi update (`bump_moved_away`), trigger `nodes_update_checksum_trigger` giống svn | #195 |
| Recovery xóa txn của commit chưa gửi MERGE: server trả 5xx hoặc đứt kết nối thì thử lại tối đa 5 lần (200 ms, nhân đôi, hủy được); 404 coi như đã xóa; hết lượt thì giữ journal. `reconcile` (MERGE có thể đã gửi) vẫn chỉ gửi DELETE một lần | #197 |
| Sparse checkout: cây thư mục chọn depth cho từng thư mục, dùng chung cho hộp thoại Checkout và `menu://wc/sparse`; `set_depth` tự đưa vào target thiếu hoặc bị exclude (như `--parents`). Browse upload/download, History so sánh hai revision (`diff_rev`), màn Welcome nhớ URL gần đây (giới hạn trong `app.sqlite`, xóa được) | #196 |
| Menu View: hiện/ẩn file unversioned và ignored, nhóm theo changelist, dạng cây (nhớ theo người dùng); Compare with branch/another folder; Help: tài liệu, thư mục log (`logs_reveal`), Report a problem (xem trước gói chẩn đoán rồi lưu zip); diff ảnh có onion skin và swipe; `?os=mac\|win` cho mock/dev; `pnpm test:browser` | #189 |
| Dialog Properties (xem/thêm/sửa/xóa, kiểm giá trị `svn:*` như `svn propset` 1.15, lệnh `props_get`); dialog Repository layout; banner `wc-busy` khi chương trình khác giữ lock, cleanup "gỡ lock của chương trình khác" chỉ chạy khi người dùng tick xác nhận và không còn tiến trình nào mở `wc.db` (Linux), không xóa file nào; banner out-of-date và tag | #190 |
| Fixture WC format 31 từ svn 1.15.0 (42 trạng thái) và f32 (43 trạng thái); repo "độc" và evil server chạy không cần Docker; e2e `hostile.test.mjs`; `xtask compat fixtures` chạy các generator; log/author có UTF-8 hỏng hiển thị bằng U+FFFD | #167 |
| Quy trình phát hành: environment có duyệt, ký số tùy chọn, tự đẩy sang `OpenSVN-releases`; `check-token-scope.sh`, release notes sinh từ conventional commit, checklist kiểm tay chữ ký và N-1→N; bundle id chốt `com.opensvn.desktop` | #169, #183, #198 |
| Benchmark 100k file so với svn 1.15, bench log/commit, crash case cho switch, soak 2 giờ, suite chạy đồng thời với svn CLI (`tests/crash/tests/concurrency.rs`) | #170 |
| URL server VisualSVN `https://host/svn/` hiện danh sách repo để chọn | #171 |
| Diff văn bản cho Word/Excel, mở phiên bản bằng app mặc định, công cụ diff ngoài theo đuôi file | #177 |
| Switch giữ con bị `exclude` như svn 1.15; switch với `--depth` hẹp để node ngoài depth (cả dòng `exclude`) ở vị trí cũ, chỉ node được bump mới dời sang vị trí mới | #179, #185 |
| `svn cleanup` của client khác giữa lúc update/checkout: transaction NODES kiểm tra file pristine dùng lại (khôi phục từ bản giữ lại hoặc dừng), kiểm tra lock còn là của mình, release không xóa row của client khác | #180 |
| Continue checkout nhận lại `.svn` rỗng hoặc chỉ có `.svn/tmp` của checkout bị kill trước khi kịp lấy lock tạo; nhận lại file svn đã đặt nhưng chưa có hàng BASE | #181, #188 |
| Settings của #17: thay pin chứng chỉ (`replacePin`), nút đăng nhập Windows, danh sách chứng chỉ tin cậy, chọn merge tool/editor/terminal, mở WC gần nhất, log level, cỡ chữ, mật độ, tab, global-ignores, xóa cache | #187 |
| Crash e2e qua app thật (SIGKILL cả process group, mở lại cùng profile), RSS với 100k file, soak 10 phút/2 giờ; sửa WC biến mất sau crash lúc commit và thiếu watcher sau khi mở lại app; soak engine giới hạn arena glibc, warm-up chờ đủ cửa sổ log | #192, #199 |
| Kiểm tra filesystem bằng `statfs` chỉ biên dịch trên Linux và trong test (clippy macOS sạch) | #194 |
| Engine update chạy cùng vòng `svn cleanup`: update xong, báo busy, hoặc dừng với lỗi có kiểu "cleaned up by another client" (`WcBusy`), không bao giờ fail vì file tạm biến mất (`WcDb::wc_lock_explain_failure`); `engine_update_vs_svn_cleanup` vào suite strict, 50/50 lần pass | #202 |
| RAM app trên Linux: app tự `exec` lại một lần khi khởi động với `MALLOC_ARENA_MAX=2` (heap thật lúc checkout chỉ ~23 MB, phần còn lại là arena glibc giữ lại); setting `appearance.hardwareAcceleration` và biến `OPENSVN_HARDWARE_ACCELERATION` đặt `WEBKIT_DISABLE_DMABUF_RENDERER`/`WEBKIT_DISABLE_COMPOSITING_MODE` (mặc định vẫn bật); app không giữ danh sách mọi notification của update (`ClientCtx::keep_events`); `rss.test.mjs` tách RSS đỉnh của app thành heap/gpu/code. 100k file: app 594 → 349 MB, cả cây 1041 → 804 MB (tắt tăng tốc: 722 MB) (`benches/results/2026-10-16-app-rss-linux.md`) | #202 |
| Màn cập nhật app (#22): Settings › Updates (phiên bản hiện tại, token chỉ ghi vào keychain, trạng thái Chưa cấu hình/Đã cấu hình/Không hợp lệ, Xóa token, `updates.autoCheck`), dialog Check for app updates… từ menu và Settings (bản mới nhất, có bản mới kèm ghi chú và Install có tiến độ rồi hỏi khởi động lại, chưa cấu hình không gửi request, lỗi kèm Retry), kiểm tra lúc mở app tối đa một lần mỗi ngày dạng toast. Backend: `update_check` trả `UpdateCheck`, `updater_token_status` trả `TokenStatus`, `app_version`, `update_restart`, sự kiện `app://update-progress`; `update_install` không tự khởi động lại | #201 |

Issue đã đóng trong đợt rà gần nhất: #9, #10, #13, #14, #99, #128, #144, #145, #146.

## 4. Đang làm (đừng nhận trùng)

| PR / nhánh | Nội dung | Issue | Trạng thái |
|---|---|---|---|
| #202 `test/finish-21` | Phần code còn lại của #21: test cleanup race vào suite strict, RAM app trên Linux, cập nhật mục 3–5 | #21 | Draft |

Chủ repo có một số nhánh làm trên máy riêng, **chưa push**: hiển thị %/ETA khi checkout, giữ nguyên byte đường dẫn (NFC/NFD), build Mac. Hỏi chủ repo trước khi làm những mảng này.

## 5. Việc còn trống (ai cũng nhận được)

Nhận việc nào thì comment vào issue tương ứng hoặc báo chủ repo, để không trùng.

1. **Nối `-g` qua `svn-ra-api::LogRequest` (#164, follow-up).** `LogProgress::accept` giờ chịu được entry merge lồng, nhưng `LogRequest` (`crates/svn-ra-api/src/session.rs:68-81`) chưa có field cho `include_merged_revisions` hay chọn revprop, và `log.rs` vẫn gán cứng `include_merged_revisions: false`. Thêm field, nối qua `log_request`/`WireLogRequest`, cập nhật IPC nếu UI cần hiện entry merge.
2. **Hiệu năng macOS/Windows (#121).** Hai nền tảng này vẫn fsync từng file (`BATCHED_SYNC` trong `crates/svn-wc/src/durable.rs`). Cần đo trên máy thật và tìm cách gộp (F_FULLFSYNC/FlushFileBuffers theo lô).
3. **Watcher và menu native (#11).** Kiểm tay menu trên macOS và Windows.
4. **Spike WebView (#12).** Chạy trên Linux (Intel/AMD/NVIDIA) và Windows 11 theo `plans/**/reports/from-dev-p09-webview-perf-spike-report.md`, rồi bỏ trạng thái "conditional" trong ADR-002.
5. **Bật `real-engine` làm feature mặc định (#20, #21).** Crash test qua app thật đã có (`e2e/crash.test.mjs`, #192). Còn bật feature mặc định, cùng ma trận compat đầy đủ, bộ test bảo mật và một tuần dùng thật (dogfood).
6. **RAM của app (#21), phần còn lại.** Số đo ở `benches/results/2026-10-16-app-rss-linux.md` (Xvfb, 100k file). Cả cây vẫn trên 400 MB: lúc idle, riêng hai process WebKit đã chiếm khoảng 385 MB RSS khi tăng tốc phần cứng bật (llvmpipe). Tắt tăng tốc thì còn 383 MB. Còn mở:
   - Đo trên máy có GPU (Intel/AMD/NVIDIA) rồi quyết định mặc định của `appearance.hardwareAcceleration` trên Linux (#182, gắn với spike WebView #12).
   - App process vẫn lên 350 MB ở refresh/update vì nhiều lượt quét toàn cây nối nhau: `wc://status-changed` làm FE gọi lại `status_get` (thêm một lượt quét) thay vì áp diff. Áp diff chỉ đúng khi danh sách của FE trùng snapshot của publisher, nên cần cho `status_get` trả snapshot.
   - `read_subtree` giải mã props của mọi hàng NODES cho status; remote check dựng `WcStatus` cho cả cây (`show_unmodified: true`) rồi mới lọc.
7. **Đếm ngược "thử lại sau Ns" đọc lại mỗi giây (#172, follow-up).** `connection-wait.tsx` đặt `role="status"` lên đoạn chữ đổi mỗi giây, nên trình đọc màn hình đọc liên tục. Nên để vùng live chỉ báo "Mất kết nối" và ẩn phần đếm ngược khỏi vùng live.
8. **"Update lên bản mới nhất" của checkout dở bỏ qua lựa chọn ignore-externals (#172, follow-up).** Nút này chạy update thường, nên tải cả externals dù checkout ban đầu chọn bỏ qua. Nên đọc journal checkout như `wc_continue`.
9. **Commit qua proxy/middlebox reset kết nối sau N request (#193, follow-up).** Với `--reset-after 2`, request ghi thứ 3 trên một kết nối luôn bị reset, nên commit không bao giờ qua được (đúng thiết kế #166: không gửi lại). Hướng sửa: gửi MERGE/LOCK/UNLOCK trên kết nối mới, hoặc giới hạn số request ghi mỗi kết nối. Cần đo chi phí handshake với VisualSVN thật.
10. **Lỗi PUT bị cắt hiện thông báo transport thô (#193, follow-up).** Txn đã được xóa nên chưa có gì được commit, nhưng người dùng chỉ thấy "Connection reset by peer". Nên bọc lỗi thành "chưa commit gì, có thể làm lại" khi DELETE txn trả về `Deleted`.
11. **Fake server chưa mô phỏng `link_path` khi update (#193, follow-up).** `svn-ra-api::fake` chỉ đọc nguồn của `link_path` khi switch. Khi update, svn_repos còn theo link ở phía đích, nên test update với subtree đã switch trên fake chưa phản ánh đúng.
12. **Switch dừng giữa chừng với `--depth` hẹp (#193, follow-up).** Khi resume neo tại thư mục đích, bump chỉ ghi lại `repos_relpath` trong phạm vi depth. Thư mục con nằm ngoài depth giữ URL cũ. App luôn switch với depth `Unknown` nên chưa gặp.
13. **`pnpm test:browser` trên WebKit (#12, follow-up).** Máy dev chưa có WebKit của Playwright nên mới chạy Chromium. Cần chạy `pnpm exec playwright-core install webkit` (hoặc dùng máy có sẵn), chạy script rồi xem ảnh trong `test-results/browser`.
14. **Mục View dạng check trong menu native (#18, follow-up).** Show unversioned/ignored, Group by changelist, Show as tree hiện là mục thường và báo trạng thái bằng toast. Nên dùng `CheckMenuItem` và đồng bộ trạng thái từ UI (cần lệnh IPC như `menu_set_layout`).
15. **Compare ở revision cụ thể hoặc tính cả thay đổi local (#18, follow-up).** Compare hiện so hai URL ở HEAD và so từng thư mục theo last-changed revision và kích thước. Muốn chính xác theo nội dung hoặc gồm working copy thì cần lệnh diff URL↔URL ở backend.
16. **Incoming edit trên node đã move (#195, follow-up).** svn 1.15 tạo tree conflict "local move away, incoming edit" và `resolve --accept mine-conflict` đẩy thay đổi sang đích. Engine mới chỉ nâng revision của đích khi nguồn không đổi (`bump_moved_away` trong `crates/svn-wc/src/db/txn.rs`).
17. **`dav_cache` (#195, follow-up).** svn lưu `svn:wc:ra_dav:version-url` vào `NODES.dav_cache`, engine để trống; scenario so sánh với `ignore: [dav_cache]`. svn tự hỏi server khi trống, nên chỉ cần làm nếu muốn wc.db giống hệt.
18. **History chờ quá lâu với revision import lớn sau authz (#192, follow-up).** Sau khi `log_get` không còn giữ working copy, `e2e/rss.test.mjs` (`OPENSVN_RSS_FILES=20000`) không còn báo refresh busy (0 lần, 0,5 s). Test giờ dừng ở bước History: không thấy `Incoming 19` sau 120 s. Trên `visualsvn-like.sh` (svn-basic, có authz), riêng `svn log -l 100` của svn 1.15 CLI đã mất 35 s, có `-v` hay không cũng vậy, vì server kiểm authz cho 20k đường dẫn của r1. Sau Update, thanh History, màn History và danh sách Incoming tải lại log cùng lúc. Cần đo số lời gọi log song song và cân nhắc gom chúng lại, hoặc nâng ngưỡng chờ của test.

## 6. Việc cần người thật (không tự động được)

- **Kiểm trên 3 OS với VisualSVN thật** (#6, #17, #19, #20): đăng nhập SSO và Basic, browse, history, blame, lock, tạo branch, checkout lớn. Dùng `docs/visualsvn-iwa-checklist.md`.
- **Kiểm chéo với TortoiseSVN** (#15, #16): OpenSVN tạo working copy rồi TortoiseSVN mở và commit, và làm chiều ngược lại.
- **Ký số và phát hành** (#22): Apple Developer ID, Authenticode, cặp khóa minisign, repo `OpenSVN-releases` và token fine-grained. Làm theo `docs/RELEASE-SETUP.md` (có sau khi #169 merge).
- **GitHub Actions không có runner:** chủ repo kiểm tra Billing/Runners của repo.
- **Bản macOS:** build trên máy Mac.

## 7. Cách phối hợp

- Mỗi việc làm trên một nhánh riêng tạo từ `origin/main` và mở draft PR sớm.
- Ghi "Closes #N" hoặc "Refs #N" trong mô tả PR.
- Main đổi thì merge main vào nhánh. Không rebase.
- Báo cáo kết quả ngay trong mô tả PR: gate nào đã chạy, số test pass/fail, việc gì chưa chạy được và lý do.
- Khi xong một mảng, cập nhật mục 3–5 của file này trong cùng PR.
