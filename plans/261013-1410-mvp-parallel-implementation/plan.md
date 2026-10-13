---
title: "OpenSVN MVP — triển khai song song nhiều agent"
description: "Kế hoạch chi tiết tới MVP desktop (Windows/Linux/macOS): engine Rust tự viết + app Tauri/React, chia gói việc có file ownership riêng để nhiều agent làm song song"
status: pending
priority: P1
branch: "main"
tags: [mvp, rust, tauri, svn, parallel]
issue: 1
blockedBy: []
blocks: []
supersedes: [261013-1101-cross-platform-svn-desktop-client]
created: "2026-10-13T07:12:26.201Z"
createdBy: "ck:plan"
source: skill
---

# OpenSVN MVP — triển khai song song nhiều agent

## Overview
Chi tiết hóa phase 1–4 của plan tổng quan [261013-1101](../261013-1101-cross-platform-svn-desktop-client/plan.md). Sau MVP có plan riêng cho: format 32 (pristines-on-demand), tối ưu ≥30% so svn CLI, merge/revision graph.
**MVP** = luồng hằng ngày trên Windows/Linux/macOS: đăng nhập VisualSVN (Basic + SSO Windows), checkout (sparse), status, diff, commit, update, revert, switch, lock, log, blame, browse, conflict cơ bản; working copy **đọc format 31/32, chỉ tạo/ghi format 31** (tương thích svn 1.8–1.15, TortoiseSVN).

Nguồn thiết kế (bắt buộc đọc trước khi làm bất kỳ phase nào):
- [docs/system-architecture.md](../../docs/system-architecture.md), [docs/adr/](../../docs/adr/)
- [docs/system-design/01](../../docs/system-design/01-ui-screens-menus-and-interactions.md) (UI), [02](../../docs/system-design/02-architecture-data-flow-and-reliability.md) (kiến trúc), [03](../../docs/system-design/03-ipc-api-contract-and-data-model.md) (IPC)
- [docs/design-guidelines.md](../../docs/design-guidelines.md), mockup [ui-mockup-main-window.html](../../docs/system-design/ui-mockup-main-window.html)
- Research: [reports/](./reports/) (protocol HTTPv2, wc.db) + 4 báo cáo red-team

## Phases

| Phase | Name | Status |
|-------|------|--------|
| 1 | [Workspace bootstrap and shared contracts](./phase-01-workspace-bootstrap-and-shared-contracts.md) | Pending |
| 2 | [svn-delta svndiff codec](./phase-02-svn-delta-svndiff-codec.md) | Pending |
| 3 | [svn-protocol sans-IO DAV layer](./phase-03-svn-protocol-sans-io-dav-layer.md) | Pending |
| 4 | [Transport auth and secure store](./phase-04-transport-auth-and-secure-store.md) | Pending |
| 5 | [svn-diff text diff and diff3](./phase-05-svn-diff-text-diff-and-diff3.md) | Pending |
| 6 | [Compat test harness and fixtures](./phase-06-compat-test-harness-and-fixtures.md) | Pending |
| 7 | [svn-wc database and pristine store](./phase-07-svn-wc-database-and-pristine-store.md) | Pending |
| 8 | [App backend core services](./phase-08-app-backend-core-services.md) | Pending |
| 9 | [Frontend shell and design system](./phase-09-frontend-shell-and-design-system.md) | Pending |
| 10 | [svn-ra remote operations](./phase-10-svn-ra-remote-operations.md) | Pending |
| 11 | [svn-client working copy operations](./phase-11-svn-client-working-copy-operations.md) | Pending |
| 12 | [Frontend feature screens wired to IPC](./phase-12-frontend-feature-screens-wired-to-ipc.md) | Pending |
| 13 | [Integration crash safety and performance](./phase-13-integration-crash-safety-and-performance.md) | Pending |
| 14 | [Packaging signing and release](./phase-14-packaging-signing-and-release.md) | Pending |
| 15 | [RealEngine integration into app](./phase-15-realengine-integration-into-app.md) | Pending |

Gói con (mỗi gói = 1 agent, 1 nhánh): **01a** contract engine · **01b** Tauri/IPC/CI · **07A/07B** · **10a** đọc · **10b** update+commit driver · **11U/11C** · **12a** remote · **12b** working copy · **15a** nối remote · **15b** nối WC.

## Waves & song song

```
Wave 0  01a contract engine (tuần 1) ── 01b Tauri/IPC/CI (tuần 1–2)
Wave 1  P02 · P03 · P04 · P05 · P06 · 07A · 07B   (sau 01a)        P08 · P09 (sau 01b)
Wave 2  10a (P03+P04) ─► 15a ─► M1 "Read-only client"       12a (P08+P09) · 12b trên FakeEngine · P14 build skeleton
Wave 3  10b (10a+P02) · 11U · 11C (07 mốc A2/B2 + 10b; trước đó chạy trên Fake) · 12b tiếp · 15b
Wave 4  P13 (crash/compat/perf/dogfood) · P14 release  ─► M3 MVP
```

| Wave | Gói chạy song song | Agent | Điều kiện bắt đầu |
|---|---|---|---|
| 0 | 01a, 01b | 2 | — (01b merge sau 01a) |
| 1 | P02, P03, P04, P05, P06, 07A, 07B · P08, P09 | 9 | 01a merge (engine) · 01b merge (P08, P09) |
| 2 | 10a, 12a, 12b (FakeEngine), 15a, P14 (skeleton) | 5 (+07A/07B còn chạy) | 10a: P03+P04 · 12a/12b: P08+P09 · 15a: 10a |
| 3 | 10b, 11U, 11C, 12b, 15b | 5 | 10b: 10a+P02 · 11U/11C: 07 mốc A2/B2 (bắt đầu trên Fake sớm hơn) · 15b: 11 |
| 4 | P13, P14 (release) | 2 | 11 + 12b + 15b |

### Mốc
| Mốc | Nội dung | P50 | P90 |
|---|---|---|---|
| M0 Contract khóa | 01a + 01b merge, wave 1 bắt đầu | tuần 2 | tuần 3 |
| M1 Read-only client (phát hành nội bộ) | đăng nhập, Browse, History, blame, lock/unlock, tạo branch/tag, cài đặt được 3 OS | tuần 9 | tuần 11 |
| M2 WC alpha | checkout/status/diff/commit/update/revert qua app trên repo thật | tuần 18 | tuần 23 |
| M3 MVP | toàn bộ acceptance criteria dưới đây | **tuần 24** | **tuần 31** |

**Đường găng:** 01a (1) → 07 bước 0 + A1–A2/B1–B2 (8) → 11 (8–9, bắt đầu trên Fake từ tuần ~7) → 15b (1–2) → P13 + dogfood (3–4). Buffer P90 ≈ +30%.

## File ownership (mỗi path chỉ 1 gói được ghi)

| Path | Owner | Ghi chú |
|---|---|---|
| `Cargo.toml` (workspace), `rust-toolchain.toml`, `deny.toml`, `.cargo/` | 01a | 01a **khai sẵn toàn bộ `[workspace.dependencies]`** dự kiến (ADR-003); thêm dep mới = contract PR |
| `Cargo.lock`, `pnpm-lock.yaml` | sinh tự động | **Không merge tay**: sau rebase chạy `cargo update -w` / `pnpm install`; CI `--locked` / `--frozen-lockfile` |
| `crates/svn-types/` | 01a | Contract sync thuần (không tokio) |
| `crates/svn-ra-api/` | 01a | Contract async `RaSession` + `FakeRaSession` (feature `fake`) |
| `crates/svn-ra/Cargo.toml`, `src/lib.rs` | 01a (tạo sẵn `pub mod transport; pub mod auth; pub mod session; pub mod ops;`) | sau đó khóa; đổi qua contract PR |
| `crates/svn-ra/src/{transport,auth}/**`, `src/session.rs` | P04 | |
| `crates/svn-ra/src/ops/{read,log,locks,file_revs,blame,open}.rs` | 10a | |
| `crates/svn-ra/src/ops/{update,commit,remote_edit}/**` | 10b | |
| `crates/opensvn-ipc/`, `apps/opensvn/` skeleton, `package.json` (root), `pnpm-workspace.yaml`, `biome.json`, `.github/workflows/ci.yml`, `.github/CODEOWNERS` | 01b | |
| `crates/svn-delta/` | P02 | |
| `crates/svn-protocol/` | P03 | |
| `crates/secure-store/` | P04 | |
| `crates/svn-diff/` | P05 | |
| `docker/`, `fixtures/`, `xtask/`, `tests/compat/{src,README.md,Cargo.toml}`, `tests/compat/scenarios/harness/`, `.github/workflows/compat.yml` | P06 | |
| `tests/compat/scenarios/<owner>/` (`protocol/`, `wc/`, `ra/`, `client/`, `e2e/`) | gói tương ứng (P03, 07A/B, 10a/b, 11U/C, 12b) | |
| `crates/svn-wc/` | 07A / 07B theo bảng module trong phase 07 | facade `src/lib.rs` do 07A sở hữu |
| `crates/svn-client/` | 11U / 11C theo bảng module trong phase 11 | facade do 11U sở hữu |
| `apps/opensvn/src-tauri/` (trừ `src/engine/real/**`, `tauri.bundle.conf.json`, `icons/`) | P08 | gồm `tauri.conf.json`, `capabilities/` |
| `apps/opensvn/src/ipc/bindings.ts` | **generator** (`pnpm gen:ipc` từ command của P08/P15) | P09 chỉ đọc; CI sinh lại và fail nếu lệch |
| `apps/opensvn/src-tauri/src/engine/real/**` | P15 (15a, 15b) | |
| `apps/opensvn/src/{app,components,ipc (trừ bindings.ts),styles,lib}/`, `apps/opensvn/package.json`, vite/tsconfig | P09 | dep FE mới cho P12 = PR nhỏ tới P09/lead |
| `apps/opensvn/src/features/{welcome,auth,settings,browse,history,checkout,sparse}/` | 12a | |
| `apps/opensvn/src/features/{changes,diff,branch,location,dialogs,conflicts,banners,activity,menus,notifications}/` | 12b | |
| `tests/crash/`, `benches/`, `.github/workflows/nightly.yml` | P13 | |
| `.github/workflows/release.yml`, `packaging/`, `apps/opensvn/src-tauri/tauri.bundle.conf.json`, `apps/opensvn/src-tauri/icons/` | P14 | bundle/updater ở file config riêng (merge bằng `--config`) |
| `docs/` | Lead | Agent đề xuất qua PR, lead duyệt |

## Quy tắc làm việc cho mọi agent
1. **Worktree + branch riêng** `feat/<gói>-<slug>` (vd `feat/07a-wc-schema`). PR vào `main`, không push thẳng `main`.
2. **Chỉ sửa path mình sở hữu.** Đổi contract (`svn-types`, `svn-ra-api`, `opensvn-ipc`, `svn-ra/src/lib.rs`, dependency mới) → PR riêng tiêu đề `contract: …` (lý do + gói bị ảnh hưởng); lead duyệt trong 24h; các gói rebase.
3. **Code theo contract, test bằng fake** (`FakeRaSession`, `RecordingEditor`, `FakeEngine`) — không chờ gói khác.
4. **CI xanh mới merge**: fmt, clippy `-D warnings`, nextest, `cargo deny check` (advisories + bans + licenses + sources), `pnpm install --frozen-lockfile`, `pnpm audit --prod`, biome, `tsc`, vitest; job compat (Docker) bắt buộc với P03, 07, 10, 11.
5. **Xung đột `Cargo.toml`**: gộp thành **một khóa** (giữ version cao hơn) — không giữ 2 dòng trùng; lockfile luôn sinh lại.
6. **Commit**: conventional commits, không nhắc AI, không plan ID/tên gói trong message hay comment code.
7. **Không secret, không tên server/tài khoản thật** trong repo; fixture dùng `svn.test.local`.
8. **GitHub Actions pin theo commit SHA**; CODEOWNERS bắt buộc lead duyệt: lockfile, workflow, crate contract.
9. **Kết thúc gói**: cập nhật checkbox phase file; PR ghi `Status: DONE | DONE_WITH_CONCERNS | BLOCKED` + tóm tắt.
10. **Rebase hằng ngày** lên `main`.

## Definition of Done (mọi gói)
- Mọi checkbox Success Criteria đạt; test chạy CI 3 OS (trừ khi phase ghi rõ chỉ Linux).
- Public API có doc comment; không `unwrap()`/`expect()` trong code thư viện (trừ test); lỗi qua `SvnError`.
- Dependency có C chỉ được phép theo danh sách ngoại lệ ADR-003: SQLite (`rusqlite` bundled), GSSAPI hệ thống (macOS/Linux, feature `negotiate`), WebKitGTK (runtime Tauri Linux). Khác → cập nhật ADR-003 trước.
- Mọi đường dẫn nhận từ server đi qua `RelPath::from_server` + `safe_join` (phase 01).

## Acceptance criteria MVP
- [ ] WC do OpenSVN tạo/sửa (format 31) dùng được bằng svn 1.14.5/1.15.0 và TortoiseSVN (status/update/commit không lỗi, không cần cleanup), và ngược lại; WC format 32 do svn 1.15 tạo **đọc** được — compat hai chiều xanh.
- [ ] Đăng nhập VisualSVN: Windows SSO (Negotiate/SSPI), macOS/Linux Basic tài khoản AD qua HTTPS; credential gắn theo origin trong OS credential store; không bao giờ gửi Basic qua `http://`.
- [ ] Luồng 10.1–10.5 [doc 01](../../docs/system-design/01-ui-screens-menus-and-interactions.md) chạy end-to-end trên 3 OS.
- [ ] Hiệu năng: **không chậm hơn svn CLI 1.15** khi checkout/update (cùng server, cùng máy); status WC 100k file lần sau < 3s.
- [ ] Crash suite (SIGKILL ngẫu nhiên + failpoint có tên, gồm "sau MERGE" và "giữa update có sửa local"): 0 hỏng WC, 0 mất thay đổi local, 0 revert ngầm thay đổi server.
- [ ] Bảo mật: CSP + Tauri capabilities tối thiểu; repo độc hại (path traversal, symlink, `.svn`, XSS trong message/tên file) không ghi ra ngoài WC, không chạy script — test xanh.
- [ ] Bản cài 3 OS (MSI, DMG, AppImage + deb) ký số khi có chứng chỉ; auto-update qua repo private riêng `OpenSVN-releases` với token chỉ đọc repo đó.

## Dependencies
- Thay thế phần chi tiết của `261013-1101-cross-platform-svn-desktop-client` (plan đó giữ làm roadmap; phase 5–6 cũ + format 32 + tối ưu ≥30% → plan sau MVP).

## Red Team Review

### Session — 2026-10-13
**Findings:** 39 thô từ 4 reviewer → gộp 16 nhóm (15 accepted, 1 rejected)
**Severity breakdown (nhóm):** 7 Critical, 7 High, 1 Medium accepted · 1 Critical rejected
Báo cáo: `reports/from-code-reviewer-to-planner-red-team-*-plan-review-report.md`

| # | Finding (nhóm) | Severity | Disposition | Applied To |
|---|---|---|---|---|
| 1 | Update có sửa local không atomic → crash revert ngầm thay đổi server | Critical | Accept | 07, 11, 13, doc 02 |
| 2 | WC_LOCK mồ côi sau crash (không PID) | Critical | Accept | 07, 08, 11 |
| 3 | Crash sau MERGE + dọn txn rò không owner | High | Accept | 01, 10, 11, 13 |
| 4 | Crash suite chỉ kill ở điểm an toàn | High | Accept | 07, 13 |
| 5 | Auth: realm-key, prompt khi giữ lock, nhiều prompt, body không gửi lại, AuthPrompter ngoài contract | Critical | Accept | 01, 04, 08, 10, doc 03 |
| 6 | Hủy < 1s không khả thi với điểm hủy hiện tại | High | Accept | 07, 08, 10, 11 |
| 7 | Nối RealEngine không có owner đúng lúc | High | Accept | 08, 11, 15 (mới) |
| 8 | Ownership/dependency/lockfile/bindings.ts | High | Accept | plan.md, 01, 04, 06, 09, 12, 13, 14 |
| 9 | Journal mode wc.db chưa xác minh vs giả định WAL | Critical | Accept | 01, 07, 08, ADR-003, doc 02 |
| 10 | Contract không compile/thiếu (tokio trong svn-types, CommitDriver, Send, WcStatus, error map, P01 quá tải) | Critical | Accept | 01, 03, 08, 10, 11 |
| 11 | Bảo mật lõi: CSP/capabilities, XSS, path từ server, href origin, TLS accept, Basic qua http | Critical | Accept | 01, 03, 04, 06, 07, 08, 09, 10, 12, doc 01 |
| 12 | Updater token lộ source, beta tự ký, supply chain | Critical | Accept (giữ GitHub Releases theo PO, tách repo `OpenSVN-releases`) | 01, 14, plan.md |
| 13 | Redaction log/diagnostics hẹp | Medium | Accept | 04, 08 |
| 14 | Lịch sai, không có bản dùng sớm, gói quá lớn | High | Accept | plan.md, 10, 12 |
| 15 | Byte-for-byte phi thực tế, C deps, gold plating, Negotiate/NTLM | High | Accept | 02, 04, 05, 07, 09, 10, 13, 14, ADR-003 |
| 16 | "svn 1.15 chưa phát hành" | Critical | **Reject** — trang tin Apache: 1.15.0 phát hành 2026-10-08. Giữ phần hợp lý: pin 1.14.5 + 1.15.0, xác minh phiên bản VisualSVN | 06 |

Quyết định PO trong phiên review: MVP chỉ ghi format 31 · MVP chỉ cần "không chậm hơn svn CLI" · updater qua GitHub Releases private (giảm rủi ro bằng repo releases riêng).

### Whole-Plan Consistency Sweep
- Decision delta: format 31 ghi-only · perf gate "không chậm hơn svn" · updater qua `OpenSVN-releases` · contract tách `svn-types`/`svn-ra-api`, commit = `CommitEditor` · journal mode wc.db giữ như svn · credential theo origin · P15 RealEngine · 01a/01b, 10a/10b, 12a/12b.
- Đã grep toàn plan + docs các thuật ngữ cũ (`CommitDriver`, `upgrade_format`, HTTP/2, `keyring v4`, 1–16 kết nối, khóa `<realm>:<user>`, WAL cho wc.db, ≥30% làm cổng MVP, kênh beta, NSIS/rpm, mergeinfo trong MVP, trait AuthPrompter ở P04) → đã sửa ở P01–P15, ADR-003, ADR-004, doc 01/02/03, system-architecture.
- Còn lại có chủ đích: mockup v1 vẫn hiển thị lựa chọn format 32 ở Checkout/Settings (thiết kế sau MVP — doc 01 §4.1 ghi MVP ẩn lựa chọn); doc 02 §1 bảng phase 6 nhắc mergeinfo (phạm vi sau MVP).
- **0 mâu thuẫn chưa giải quyết.**

## Open questions
- VisualSVN Server nội bộ chạy mod_dav_svn 1.14 hay 1.15? Người dùng truy cập bằng FQDN có SPN hay short name/IP (quyết định cần NTLM fallback không)?
- Link mở revision trên VisualSVN web chưa đối chiếu (mẫu `#{repo}/commit/r{rev}/`).
- Chứng chỉ ký Apple Developer ID / Authenticode chưa có (P14 phát hành nội bộ không ký nếu thiếu).
- Có cần dùng chung cache credential với svn CLI/TortoiseSVN (định dạng realmstring svn) không?
