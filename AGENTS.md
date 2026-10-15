# AGENTS.md: hướng dẫn và tiến độ cho người và agent cùng làm OpenSVN

Đọc file này trước khi nhận việc. Lệnh build/test, quy tắc đặt tên nhánh và cấu trúc repo nằm ở `docs/contributing.md`. Kiến trúc nằm ở `docs/system-architecture.md`.

Cập nhật lần cuối: 2026-10-15, main tại `56d1f06`.

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
| `LogProgress::accept` chịu được entry merge lồng (`-g`): entry con không còn kéo lệch `last`, không bị bỏ hay lặp khi tải tiếp | #164 (follow-up) |
| Mất mạng lâu khi checkout/update: chờ mạng (`network.reconnectWaitMin`, mặc định 10 phút) rồi tải tiếp đúng revision; "Tiếp tục checkout" sau khi hủy, đóng app hoặc crash (journal checkout); phát hiện ngủ/thức bằng đồng hồ boot, không theo giờ hệ thống | #172 |

Issue đã đóng trong đợt rà gần nhất: #9, #10, #13, #14, #99, #128, #144, #145, #146.

## 4. Đang làm (đừng nhận trùng)

| PR / nhánh | Nội dung | Issue | Trạng thái |
|---|---|---|---|
| #171 `feat/parent-path-repo-list` | URL server VisualSVN `https://host/svn/` hiện danh sách repo để chọn | #133 | Đang review |
| #169 `ci/release-gating-publish` | Quy trình phát hành: environment có duyệt, ký số tùy chọn, tự đẩy sang `OpenSVN-releases` | #22 | Đang review |
| #170 `perf/bench-100k-crash-soak` | Benchmark 100k file với svn 1.15, crash test cho switch, soak 2 giờ, test chạy đồng thời với svn CLI | #121, #21 | Đang code |
| #192 `test/app-crash-rss-soak` | Crash e2e qua app thật (SIGKILL cả process group, relaunch cùng profile), RSS với 100k file, soak 10 phút/2 giờ; svn 1.15 cho nightly; svn 1.14.5 dùng WC do engine tạo. Kèm 2 bản sửa: WC biến mất sau crash lúc commit, không có watcher sau khi mở lại app | #20, #21 | Draft |
| #167 `test/fixtures-hostile-repo` | Fixture WC format 31 từ svn 1.15 (42 trạng thái) và f32 (43 trạng thái); repo "độc" + evil server chạy không cần Docker; e2e `hostile.test.mjs`; `xtask compat fixtures` chạy các generator; log/author có UTF-8 hỏng hiển thị bằng U+FFFD | #8, #18 | Đang review |

Chủ repo có một số nhánh làm trên máy riêng, **chưa push**: xem/diff file Word/Excel, hiển thị %/ETA khi checkout, giữ nguyên byte đường dẫn (NFC/NFD), build Mac. Hỏi chủ repo trước khi làm những mảng này.

## 5. Việc còn trống (ai cũng nhận được)

Nhận việc nào thì comment vào issue tương ứng hoặc báo chủ repo, để không trùng.

1. **Nối `-g` qua `svn-ra-api::LogRequest` (#164, follow-up).** `LogProgress::accept` giờ chịu được entry merge lồng, nhưng `LogRequest` (`crates/svn-ra-api/src/session.rs:68-81`) chưa có field cho `include_merged_revisions` hay chọn revprop, và `log.rs` vẫn gán cứng `include_merged_revisions: false`. Thêm field, nối qua `log_request`/`WireLogRequest`, cập nhật IPC nếu UI cần hiện entry merge.
2. **Lock bị lỗi connect báo `WriteOutcomeUnknown` (#166, follow-up).** Lỗi connect nghĩa là yêu cầu chưa được gửi, nên phải báo lỗi mạng thường. Sửa nhỏ ở `outcome_unknown` trong `apps/opensvn/src-tauri/src/engine/real/locks.rs` (có thể cần `svn-ra` phân biệt lỗi connect).
3. **Hiệu năng macOS/Windows (#121).** Hai nền tảng này vẫn fsync từng file (`BATCHED_SYNC` trong `crates/svn-wc/src/durable.rs`). Cần đo trên máy thật và tìm cách gộp (F_FULLFSYNC/FlushFileBuffers theo lô).
4. **Watcher và menu native (#11).** Kiểm tay menu trên macOS và Windows.
5. **Spike WebView (#12).** Chạy trên Linux (Intel/AMD/NVIDIA) và Windows 11 theo `plans/**/reports/from-dev-p09-webview-perf-spike-report.md`, rồi bỏ trạng thái "conditional" trong ADR-002.
6. **Bật `real-engine` làm feature mặc định (#20, #21).** Crash test qua app thật đã có (`e2e/crash.test.mjs`, #192). Còn bật feature mặc định, cùng ma trận compat đầy đủ, bộ test bảo mật và một tuần dùng thật (dogfood).
7. **RAM của app (#21).** Đo bằng `e2e/rss.test.mjs`, `e2e/soak.test.mjs` (#192). Dưới Xvfb, cả cây tiến trình đã 582 MB RSS khi app vừa mở, trong đó `WebKitWebProcess` chiếm 344 MB. `WEBKIT_DISABLE_DMABUF_RENDERER=1` hạ còn 389 MB. Cần đo trên máy có GPU và quyết định mặc định cho Linux, gắn với spike WebView (#12).
8. **Lỗi log ở thanh bên History bị nuốt (#167, follow-up).** `wc-sidebar.tsx` hiện danh sách rỗng khi `log_get` lỗi, không có thông báo hay nút Thử lại. Màn History chính đã làm đúng; thanh bên cần làm giống vậy.
9. **Lỗi giao thức của server bị gắn mã `wc-needs-cleanup` (#167, follow-up).** Khi server trả XML hỏng (`ErrorCode::Corrupt` trong svn-protocol), app báo cần Cleanup working copy. Cần mã lỗi riêng cho lỗi phía server.
10. **Browse giấu tên không an toàn mà không báo (#167, follow-up).** Mục bị `RelPath::from_server` từ chối (`<`, `:`, `|`, `CON`…) bị bỏ đi, nên thư mục hiện "This folder is empty". Cần báo "N mục bị ẩn"; repo "độc" (`names/`) tái hiện được.
11. **"Retry now" bấm khi thao tác chưa chờ mạng (#172, follow-up).** `OperationManager::retry_now` gọi `Notify::notify_one`, nên một lần bấm (hoặc một lần thức dậy) khi thao tác đang chạy bình thường để lại permit; lần mất mạng sau bỏ qua khoảng chờ đầu tiên. Vô hại nhưng nên chỉ đánh thức khi đang chờ.
12. **Đếm ngược "thử lại sau Ns" đọc lại mỗi giây (#172, follow-up).** `connection-wait.tsx` đặt `role="status"` lên đoạn chữ đổi mỗi giây, nên trình đọc màn hình đọc liên tục. Nên để vùng live chỉ báo "Mất kết nối" và ẩn phần đếm ngược khỏi vùng live.
13. **"Update lên bản mới nhất" của checkout dở bỏ qua lựa chọn ignore-externals (#172, follow-up).** Nút này chạy update thường, nên tải cả externals dù checkout ban đầu chọn bỏ qua. Nên đọc journal checkout như `wc_continue`.
14. **`log_get` giữ khóa đọc working copy suốt lời gọi mạng (#192, follow-up).** `commands/remote.rs` chạy `engine.log` cho `LogTarget::Wc` bên trong `wc_read`. Thanh History tải log ngay khi mở app. Một revision có nhiều đường dẫn thay đổi (import 50k–100k file, Apache có authz) làm REPORT log mất 40–90 s và được gọi lại liên tục. Trong lúc đó, một thao tác ghi xếp hàng (mở WC, Update) chặn mọi lần đọc, nên refresh và danh sách báo busy. Update trong soak 50k file không bao giờ bắt đầu. Cần chỉ tra vị trí node dưới khóa đọc, rồi gọi log qua mạng ngoài khóa.

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
