# OpenSVN

SVN client kiểu GitHub Desktop cho Windows, Linux, macOS (dùng nội bộ; iPadOS/Android để sau).
Engine SVN viết bằng Rust thuần (không dùng svn CLI/libsvn), UI Tauri 2 + React.
Server mục tiêu: VisualSVN Server (HTTPS), đăng nhập tài khoản AD.
Working copy tương thích svn chuẩn (`.svn/wc.db` format 31/32) — dùng chung với TortoiseSVN, svn CLI.

## Tài liệu
- Nghiên cứu SmartSVN: [plans/reports/research-261013-1101-smartsvn-github-desktop-style-svn-client-report.md](plans/reports/research-261013-1101-smartsvn-github-desktop-style-svn-client-report.md)
- Kế hoạch: [plans/261013-1101-cross-platform-svn-desktop-client/plan.md](plans/261013-1101-cross-platform-svn-desktop-client/plan.md)
- Kiến trúc & ADR: [docs/system-architecture.md](docs/system-architecture.md)
- System design (UI, menu, kiến trúc, IPC): [docs/system-design/](docs/system-design/)
- Mockup: [docs/system-design/ui-mockup-main-window.html](docs/system-design/ui-mockup-main-window.html)

## Build & Dev

Yêu cầu: Node 24, pnpm 11, Rust theo `rust-toolchain.toml`; Linux cần `libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev libssl-dev libxdo-dev patchelf`.

```bash
pnpm install --frozen-lockfile
pnpm dev                       # tauri dev (Vite 127.0.0.1:1420)
pnpm gen:ipc                   # sinh lại apps/opensvn/src/ipc/bindings.ts từ Rust (CI fail nếu lệch)
pnpm exec biome check .        # lint + format
pnpm typecheck && pnpm test    # tsc --noEmit, vitest
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked
pnpm tauri build --debug --no-bundle
```

Contract IPC: `crates/opensvn-ipc` (kiểu `serde` + `specta`), command stub trong `apps/opensvn/src-tauri`.

Phiên bản đã pin và kiểm tra build cùng nhau (2026-10-13):

| Thành phần | Phiên bản |
|---|---|
| tauri / tauri-build | 2.12.3 / 2.7.2 |
| tauri-specta / specta | =2.0.0-rc.25 (cùng) |
| specta-typescript | =0.0.12 |
| @tauri-apps/api / cli | 2.12.3 / 2.12.2 |
| react / react-dom | 19.3.0 |
| vite / @vitejs/plugin-react | 8.3.4 / 6.1.2 |
| typescript | 7.0.2 |
| vitest | 5.0.4 |
| @biomejs/biome | 2.5.15 |

`specta`/`tauri-specta` còn là bản rc: luôn pin bằng `=`.

## Quy ước mã nguồn
- Mã port từ Apache Subversion giữ nguyên header ASF, thêm dòng "Modified by OpenSVN".
- Cập nhật `NOTICE` trong cùng PR khi port thêm mã hoặc SQL từ upstream (Apache-2.0).
- Giấy phép của OpenSVN chưa chọn (quyết định của PO); repo hiện là nội bộ.

## Trạng thái
Skeleton app + contract IPC + CI; engine đang phát triển.
