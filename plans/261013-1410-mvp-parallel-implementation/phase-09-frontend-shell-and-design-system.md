---
phase: 9
title: "Frontend shell and design system"
status: pending
priority: P1
effort: "3 tuần · 1 agent"
dependencies: [1]   # cần gói 01b
wave: 1
owner_paths: ["apps/opensvn/src/{app,components,ipc (trừ bindings.ts — generator),styles,lib}/", "apps/opensvn/package.json", "apps/opensvn/vite.config.ts", "apps/opensvn/tsconfig*.json", "apps/opensvn/index.html"]
---

# Phase 9: Frontend shell and design system

## Overview
Khung giao diện React theo [design-guidelines.md](../../docs/design-guidelines.md) và [mockup v1](../../docs/system-design/ui-mockup-main-window.html): token 2 theme, component nền, bố cục cửa sổ chính, IPC client + mock adapter, và **spike hiệu năng WebView** (quyết định giữ Tauri hay chuyển Electron — ADR-002).

## Requirements
- Functional:
  - Tailwind CSS v4 + shadcn/ui; token CSS đúng bảng §2 design-guidelines (map shadcn §2.3), theme system/light/dark; font IBM Plex đóng gói local.
  - Component (design-guidelines §6): Button (default/primary/danger), ToolbarItem (2 dòng + badge), Tabs, SegmentedControl, Chip, Input/Select/Textarea, FileRow, RevisionRow, StatusGlyph, LockBadge, Banner, DropdownMenu, ContextMenu, Popover, Dialog, Sheet, Toast, ActivityRow, StatusBar, EmptyState.
  - Bố cục: Toolbar (Working copy ▾ · Branch/Location ▾ · Update · Activity), Banner slot, Sidebar (tabs + filter + list + commit box) kéo giãn 220–600px, Main pane, Activity panel, Status bar.
  - Virtualized list (TanStack Virtual) cho file/revision; DiffView wrapper CodeMirror 6 + `@codemirror/merge` (split/unified, hide whitespace, language theo đuôi file).
  - IPC client: wrapper `bindings.ts` + TanStack Query hooks (`useWorkingCopies`, `useStatus`, `useLog`…), event bus (`op://*`, `wc://*`, `auth://prompt`, `menu://*`) invalidate query.
  - **Mock adapter**: chạy FE trong trình duyệt (`pnpm dev:mock`) với dữ liệu mẫu giống mockup, không cần Tauri — để P12 và review UI làm song song.
  - i18n (`i18next`, vi mặc định, en), phím tắt theo OS (`Mod` = ⌘/Ctrl) từ bảng doc 01 §8.
  - Trang `/__gallery` (chỉ build dev) hiển thị mọi component 2 theme để review — không phải cổng CI.
  - **An toàn nội dung**: lint Biome/ESLint rule cấm `dangerouslySetInnerHTML`, `innerHTML`, `eval`; component `SafeText`, `SafeLink` (chỉ `https?://`, mở trình duyệt ngoài); ảnh/SVG chỉ qua `<img>` (không iframe/object/embed); CodeMirror ở chế độ text thuần (không render HTML/markdown).
- Non-functional: list 100k dòng cuộn 60 fps; diff 20k dòng mở < 300ms; a11y: focus ring, ARIA, tương phản AA (axe check trong test).

## Architecture
```
apps/opensvn/src/
  app/{App.tsx, routes.tsx, providers.tsx, shortcuts.ts}
  components/{ui/*  (shadcn-based), layout/*, list/*, diff/*}
  ipc/{bindings.ts (generated), client.ts, events.ts, queries.ts, mock/{adapter.ts, data.ts}}
  styles/{tokens.css, fonts.css, globals.css}
  lib/{i18n.ts, format.ts (bytes, thời gian tương đối, revision), platform.ts}
  features/   (P12 sở hữu — để trống)
```

## Related Code Files
- Create: như trên. Không sửa `src-tauri/`, `features/`.

## Implementation Steps
1. Tailwind v4 + shadcn init; `tokens.css` từ design-guidelines (light/dark/system) + fonts local.
2. **Spike WebView (tuần 1, ưu tiên cao)**: trang `/__perf` với list ảo 100k dòng + CodeMirror merge 20k dòng; đo fps/RAM trên macOS (WKWebView), Windows (WebView2), Linux Ubuntu 24.04 + Fedora (WebKitGTK; GPU Intel/AMD/NVIDIA nếu có). Ghi kết quả vào PR → lead cập nhật ADR-002 (giữ Tauri / chuyển Electron).
3. Component nền + `/__gallery`.
4. Layout cửa sổ chính + resizable sidebar + Activity/Status bar.
5. IPC client + TanStack Query + event bus + mock adapter (`pnpm dev:mock`).
6. DiffView (CodeMirror 6) + ImageCompare (side-by-side/onion/swipe).
7. i18n + shortcuts + platform detection.
8. Test: Vitest + Testing Library cho component; axe a11y; test XSS: chuỗi `<script>`/`<img onerror>` trong prop component hiển thị dạng text.

## Success Criteria
- [ ] `/__gallery` có đủ component §6 ở 2 theme, khớp mockup v1 (lead review bằng mắt).
- [ ] Lint cấm HTML injection bật trong CI; test XSS component xanh.
- [ ] Kết quả spike WebView 3 OS được ghi và ADR-002 được chốt.
- [ ] `pnpm dev:mock` hiển thị cửa sổ chính với dữ liệu mẫu, đổi theme/OS-shortcut được.
- [ ] Ngưỡng hiệu năng list/diff đạt trên 3 OS (hoặc lý do + quyết định nếu Linux không đạt).
- [ ] axe: 0 lỗi serious/critical.

## Risk Assessment
- WebKitGTK chậm/lỗi trên NVIDIA → phát hiện tuần 1; nếu chuyển Electron, chỉ đổi vỏ (`src-tauri` ↔ electron main) vì FE dùng adapter IPC.
- shadcn/Tailwind v4 thay đổi API → pin version.
- CodeMirror merge với file rất lớn → giới hạn: > 5MB hỏi trước (đã có trong IPC `tooLarge`).
