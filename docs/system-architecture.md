# Kiến trúc hệ thống OpenSVN

Cập nhật: 2026-10-13

Phạm vi: **Windows, Linux, macOS**. iPadOS/Android để sau (kiến trúc giữ đường mở).

## Tổng quan
```
┌──────────────────── Tauri 2 app (Windows · Linux · macOS) ─────────────────────┐
│ WebView: React 19 + TS                                                         │
│   shadcn/ui · TanStack Query/Virtual · CodeMirror 6 merge · Zustand            │
│                  ▲ tauri-specta (type TS sinh tự động)                          │
│                  │ command / event (progress stream, cancel)                    │
│ Rust: src-tauri  (queue thao tác theo WC, secure-store, FS watcher)            │
│   └── svn-client   checkout/update/commit/revert/switch/merge                  │
│        ├── svn-wc      .svn/wc.db chuẩn libsvn_wc (format 31/32), pristine     │
│        ├── svn-ra      RA API qua HTTP(S): info/list/log/lock/update/commit    │
│        │    └── svn-protocol (sans-IO DAV/HTTPv2) + transport reqwest/rustls   │
│        ├── svn-delta   svndiff0/1/2, delta editor                              │
│        └── svn-diff    diff (imara-diff), diff3, mergeinfo                     │
└──────────────────────────────── HTTPS ─────────────────────────────────────────┘
                                   ▼
             VisualSVN Server (mod_dav_svn, AD: Basic + Negotiate)
```
Working copy dùng chung được với svn CLI / TortoiseSVN / SmartSVN (ADR-004). Dữ liệu riêng của app (cache log, layout branch, shelve) nằm trong thư mục dữ liệu ứng dụng, không ghi vào `.svn/`.

## Bộ system design
| Tài liệu | Nội dung |
|---|---|
| [01 — Giao diện, menu & tương tác](system-design/01-ui-screens-menus-and-interactions.md) | Sitemap, cửa sổ chính, dialogs, context menu, menu bar, phím tắt, settings, luồng chính |
| [02 — Kiến trúc, luồng dữ liệu & độ tin cậy](system-design/02-architecture-data-flow-and-reliability.md) | Yêu cầu, thành phần, đồng thời, luồng status/update/commit, cache, lỗi, bảo mật, quy mô |
| [03 — Hợp đồng IPC & mô hình dữ liệu](system-design/03-ipc-api-contract-and-data-model.md) | Kiểu TS, commands, events, mã lỗi, schema app.sqlite/cache.sqlite, settings |
| [Mockup cửa sổ chính](system-design/ui-mockup-main-window.html) | HTML tương tác (mở trực tiếp trong trình duyệt) |
| [Design system v1](design-guidelines.md) | Token màu/chữ/spacing, component, glyph trạng thái, quy ước nền tảng |

## Quyết định (ADR)
| ADR | Nội dung | Trạng thái |
|---|---|---|
| [ADR-001](adr/adr-001-svn-engine-language-rust.md) | Engine viết bằng Rust, ưu tiên pure Rust | Accepted |
| [ADR-002](adr/adr-002-ui-framework-tauri-react.md) | UI: Tauri 2 + React/TS, CodeMirror 6; dự phòng Electron | Accepted (chờ spike Linux) |
| [ADR-003](adr/adr-003-rust-core-library-stack.md) | Thư viện engine: tokio, reqwest, rustls-platform-verifier, quick-xml, rusqlite, cross-krb5, keyring... | Accepted |
| [ADR-004](adr/adr-004-working-copy-format.md) | Tương thích `.svn/wc.db`: MVP đọc 31/32, ghi 31; ghi 32 (pristines on demand) sau MVP | Accepted (cập nhật 2026-10-13) |

## Frontend
| Nhu cầu | Chọn |
|---|---|
| Framework | React 19 + TypeScript (strict) + Vite |
| UI kit | shadcn/ui (Radix + Tailwind CSS v4), lucide icons |
| Dữ liệu async từ Rust | TanStack Query |
| State UI cục bộ | Zustand |
| List/cây lớn | TanStack Virtual |
| Diff/merge/blame | CodeMirror 6 + `@codemirror/merge` |
| So sánh ảnh | component tự viết (side-by-side, onion, swipe) |
| Revision graph (phase 6) | `elkjs` layout + SVG/Canvas |
| i18n | `i18next` (vi, en) |
| Test | Vitest + Testing Library; E2E `tauri-driver` (Windows/Linux) |

## Công cụ & hạ tầng
| Hạng mục | Chọn |
|---|---|
| Repo & CI | GitHub + GitHub Actions; matrix `windows-latest`, `ubuntu-latest`, `macos-latest`; cache `Swatinem/rust-cache` |
| Monorepo | Cargo workspace + pnpm workspace |
| Lint/format | clippy, rustfmt, cargo-deny; Biome (TS) |
| Môi trường test | Docker (Linux runner): Apache httpd + mod_dav_svn + svn CLI 1.14 & 1.15; VisualSVN Server test nội bộ cho kiểm thử thủ công/định kỳ |
| Máy dev | macOS (dev chính); Windows/Linux qua CI + VM |
| Đóng gói | Windows: MSI ký Authenticode; macOS: DMG ký Developer ID + notarize; Linux: AppImage + .deb |
| Cập nhật | `tauri-plugin-updater` từ GitHub Releases của repo private riêng `OpenSVN-releases` (token fine-grained chỉ đọc repo đó) |
| Mobile (sau) | iPadOS qua TestFlight; Android APK |
| Bảo mật | Mật khẩu chỉ trong OS credential store; log lọc credential; TLS luôn verify |
