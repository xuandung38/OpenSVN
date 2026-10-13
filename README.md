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

## Trạng thái
Thiết kế — chưa có code.
