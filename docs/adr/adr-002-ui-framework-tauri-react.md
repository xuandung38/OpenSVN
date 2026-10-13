# ADR-002: Framework UI desktop — Tauri 2 + React/TypeScript

**Status:** Accepted (có điều kiện: spike WebKitGTK Linux ở phase 1)
**Date:** 2026-10-13
**Deciders:** Tech lead, FE lead

## Context
- Phạm vi hiện tại: **Windows, Linux, macOS**. iPadOS/Android để sau — không chọn công nghệ chặn đường mobile.
- Engine Rust (ADR-001).
- Màn hình nặng: danh sách 100k file, cây thư mục, diff/merge, log dài, blame, revision graph (sau).
- Cần UX desktop thật: phím tắt, context menu, kéo-thả, nhiều cửa sổ.

## Decision
**Tauri 2** (Rust core cùng process) + **React 19 + TypeScript + Vite**.
- Diff/merge: **CodeMirror 6 + `@codemirror/merge`** (nhẹ, tùy biến tốt, chạy được cả trên mobile sau này — Monaco không hỗ trợ mobile).
- List/cây lớn: **TanStack Virtual**.
- Gọi engine: Tauri command + event (stream progress), type TS sinh bằng **tauri-specta**.

WebView theo nền tảng: Windows = WebView2 (Chromium), macOS = WKWebView, Linux = WebKitGTK.

## Options Considered

### Option A: Tauri 2 + React (chọn)
| Dimension | Assessment |
|---|---|
| Complexity | Med |
| Cost | Không phí |
| Scalability | Tốt với virtualization |
| Team familiarity | Cao |

**Pros:** Rust gọi trực tiếp; app nhỏ (~10–20MB); hệ sinh thái web mạnh nhất cho diff/editor/virtual list; cùng codebase build được iOS/Android khi cần.
**Cons:** WebKitGTK trên Linux có lỗi hiển thị với một số driver NVIDIA (DMABUF); khác biệt render WebKit vs Chromium.

### Option B: Electron + napi-rs (phương án dự phòng)
| Dimension | Assessment |
|---|---|
| Complexity | Med |
| Cost | Không |
| Scalability | Tốt, Chromium đồng nhất mọi OS |
| Team familiarity | Cao |

**Pros:** render đồng nhất, desktop rất trưởng thành (VS Code, GitHub Desktop dùng Electron). Cùng code React → chuyển đổi rẻ.
**Cons:** app ~150MB+, RAM cao; Rust qua native addon (napi-rs); không có đường lên mobile.

### Option C: Flutter + flutter_rust_bridge
**Pros:** render nhất quán, có mobile. **Cons:** widget diff/editor yếu, desktop UX (multi-window, menu) hạn chế, Dart.

### Option D: UI native (WinUI + SwiftUI + GTK) + UniFFI
**Cons:** 3 codebase UI.

### Loại sớm
Slint, egui, Dioxus: text editor/diff và hệ sinh thái component chưa đủ cho app dạng này.

## Trade-off Analysis
Diff/list lớn → web thắng. Giữa Tauri và Electron: Tauri nhẹ hơn nhiều, tích hợp Rust tự nhiên và giữ đường mobile; Electron chỉ hơn ở render đồng nhất. Rủi ro Tauri tập trung ở Linux → kiểm chứng bằng spike; vì code React dùng chung nên chuyển sang Electron nếu cần chỉ tốn phần vỏ.

## Consequences
- Dễ hơn: tuyển FE; diff/merge chất lượng; phát hành mobile sau không phải viết lại UI.
- Khó hơn: test trên 3 engine web (Chromium/WebKit/WebKitGTK); E2E WebDriver chỉ chạy Windows/Linux (không có driver cho WKWebView).
- Tiêu chí chuyển sang Electron: spike Linux (Intel/AMD/NVIDIA) cuộn list 100k dòng < 50 fps, diff 20k dòng giật, hoặc lỗi render không workaround được.

## Action Items
1. [ ] Spike Linux (Ubuntu 24.04, Fedora; GPU Intel/AMD/NVIDIA) + Windows 11 + macOS: list ảo 100k dòng, CodeMirror merge 20k dòng, đo fps/RAM.
2. [ ] Cấu hình tauri-specta.
3. [ ] UI kit: shadcn/ui (Radix + Tailwind), dark mode.
