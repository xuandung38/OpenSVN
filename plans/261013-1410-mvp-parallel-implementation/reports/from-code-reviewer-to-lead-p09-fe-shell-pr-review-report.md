# Review PR #41: P09 FE shell M1

Ngày: 2026-10-13 · Reviewer: code-reviewer · HEAD `a362949` (2 commit trên origin/main) · worktree `OpenSVN-worktrees/review-pr41`

## Verdict: REQUEST CHANGES

Không có lỗi CRITICAL. Có 5 lỗi IMPORTANT phải sửa trước khi merge (event bus leak/double-dispatch, mock adapter lọt vào bản production và tự fallback, chưa chuẩn hoá lỗi thành AppError, SafeLink không có opener cho Tauri, listbox không dùng được bằng bàn phím). Phần contrast là quyết định của PO: các con số dev báo là đúng, và còn nhiều cặp màu fail hơn dev báo.

## Gates (chạy local + CI)
| Gate | Kết quả |
|---|---|
| `pnpm install --frozen-lockfile` | OK (lock khớp) |
| `biome check .` | OK, 73 file |
| `tsc --noEmit` | OK |
| `vitest run` | 29/29 pass (gồm cả test chạy lint:safety) |
| `lint:safety` | ok |
| `pnpm build` | OK, 1 chunk JS 422.6 kB (gzip 134 kB) |
| `pnpm audit --prod` | 0 vuln |
| `gh pr checks 41` | 11/11 pass, có web/rust/tauri build windows-2025 |

## Đã xác minh, không có vấn đề
- Bản `dist/` production không có `__perf`, `__gallery`, codemirror, perf code (grep bundle cho ra 0). Gate `import.meta.env.DEV`/`VITE_PERF_SPIKE` bị cắt hẳn khi build.
- Code app không có `dangerouslySetInnerHTML`/`innerHTML`/`eval`/`new Function`/`document.write`. 5 chỗ `innerHTML` trong bundle đều nằm trong nội bộ React DOM.
- CSP `style-src 'self'`: `style={{...}}` của React/Radix/TanStack Virtual đi qua CSSOM nên được phép. Code chèn `<style>` của react-style-singleton (react-remove-scroll) có trong bundle nhưng không bao giờ chạy: DropdownMenu/ContextMenu dùng `modal={false}`, Popover mặc định không modal, còn Dialog/Sheet dùng `<dialog>` native. Dev chọn đúng.
- Font: toàn bộ `url()` trong CSS trỏ `/assets/*.woff2`, `assetsInlineLimit: 0`, không có font từ CDN/mạng.
- Token §2.1: 18 token × 2 theme khớp đúng từng hex với design-guidelines. Logic theme đúng: `:root`/`[data-theme=light]` → media dark `:root:not([data-theme=light])` → `[data-theme=dark]`. Đã kiểm specificity: chọn light khi hệ thống đang dark vẫn giữ light.
- IPC: `Commands = typeof commands` lấy từ `bindings.ts`. Type payload của event bus import từ bindings (là alias của `OpFailedEvent=OpFailed`...). Không có type nào viết tay trùng lặp.
- Repo strings (path, message, author, error, toast) đều render dạng text node. Test XSS phủ FileRow/RevisionRow/Banner/EmptyState/ActivityRow/Dialog/Toast.
- License prod: MIT/ISC/Apache/BSD/0BSD/MPL-2.0 (lightningcss)/OFL-1.1 (font), đều nằm trong allowlist deny.toml.

## IMPORTANT

### I1. Event bus: `stop()` gọi trước khi `start()` xong thì listener rò rỉ hoặc nhận đúp
`src/ipc/events.ts:96-110`, `src/app/providers.tsx:23-32`
`start()` await `listen()` lần lượt cho từng tên event. `stop()` chỉ gỡ những gì đã kịp push vào `unlistens`. StrictMode (dev) chạy mount, cleanup, mount ngay lập tức nên cleanup gặp mảng rỗng và lần start thứ hai đăng ký chồng lên. **Đã chứng minh bằng test tạm:** start, stop, start, sau đó emit `auth://prompt` 1 lần thì handler bị gọi **2 lần**. Nếu Providers unmount hẳn khi start còn đang chạy (Tauri `listen` là một IPC round-trip) thì listener rò rỉ vĩnh viễn. Cờ `cancelled` chỉ chặn được `console.error`. Khi nối dialog auth hay toast vào bus thì UI sẽ bật đúp.
Fix: thêm cờ `stopped`/generation trong closure. Sau mỗi `await adapter.listen(...)`, nếu bus đã stop thì gọi `u()` ngay rồi return. `start()` gọi lặp lại phải idempotent (trả về cùng một promise). Thêm test "stop trước khi start resolve → 0 handler".

### I2. Mock adapter nằm trong bundle production, và production tự fallback sang mock
`src/ipc/client.ts:3,15-20`
`mockAdapter` được import tĩnh. Bundle production chứa `mock-op-` và `mock: command "…" is not implemented`, nghĩa là có cả dữ liệu WC/log mẫu. `selectAdapter()` trả về mock mỗi khi thiếu `__TAURI_INTERNALS__`, kể cả trong bản release, nên lỗi bridge sẽ hiện ra dưới dạng "working copy giả" thay vì báo lỗi.
Fix: `if (import.meta.env.DEV || import.meta.env.MODE === "mock")` thì `await import("./mock/adapter")`. Ngược lại luôn dùng `tauriAdapter`, nếu thiếu bridge thì throw hoặc hiện EmptyState lỗi. Thêm bước kiểm bundle (grep `mock-op` trong `dist` ở CI web job, hoặc dùng `define` + test).

### I3. Lỗi không phải AppError đi thẳng vào UI
`src/ipc/client.ts:22-38`, `bindings.ts:633-639` (`typedError`)
`typedError` ép **mọi** giá trị reject không phải `Error` thành `AppError`. Lỗi cấp framework của Tauri (sai tham số, command không tồn tại, panic) reject bằng **string**, nên `appError = "invalid args…"`, `appError.message === undefined` và `IpcError.message === ""`. Toast ở `app-shell.tsx` (`toast.show(e.message)`) khi đó hiện chuỗi rỗng, và code P12 đọc `appError.code` sẽ nhận `undefined`. Doc 03 dòng 10 ghi "Mọi lỗi theo AppError. Không throw string."
Fix: trong `unwrap`, thêm `toAppError(e)`. Nếu là object có `code` và `message` dạng string thì giữ nguyên, còn lại map sang `{code:"internal", message:String(e), detail:null, path:null, retryable:false, actions:null}`. Đồng thời bọc `try/catch` quanh `await call` cho trường hợp throw `Error`. Thêm test với một promise reject bằng string.

### I4. SafeLink/openExternalUrl: production Tauri rơi vào `window.open`
`src/lib/external-link.ts:7-9`
`setExternalOpener` không được gọi ở đâu ngoài test. Chạy trong Tauri, link sẽ đi `window.open`. Tuỳ engine, link có thể bị chặn im lặng (WKWebView không có UI delegate) hoặc mở popup WebView2/WebKitGTK tải trang remote ngay trong app, không phải trình duyệt hệ thống. Cả hai đều trái plan ("mở trình duyệt ngoài"). Hiện SafeLink chỉ có trong gallery nên bị tree-shake, nhưng P12 (link revision VisualSVN) sẽ dính lỗi này.
Fix: khi `isTauriRuntime()`, opener mặc định phải là command backend (`tauri-plugin-opener` với scope `https://*`, hoặc command P08 validate scheme lại ở Rust). Fallback `window.open` chỉ dùng cho `DEV`/mock. Khi chạy production mà không có opener thì throw, không gọi `window.open`. Phụ: `<a href>` vẫn bắt được middle-click/auxclick và kéo-thả. Nên chặn `onAuxClick` hoặc render `role="link"` không có `href`.

### I5. Listbox file/revision không thao tác được bằng bàn phím
`src/components/list/virtual-list.tsx:40`, `file-row.tsx:36`, `revision-row.tsx:32`
Container `role="listbox"` không có `tabIndex` và không xử lý phím mũi tên. Các option có `tabIndex={-1}` nên `onKeyDown` (Enter/Space) trên hàng không bao giờ chạy. Chỉ checkbox trong hàng là Tab tới được, và focus mất khi hàng bị virtualizer unmount. Không chọn được file hay revision bằng bàn phím, trái §8 "Mọi chức năng đi tới được bằng bàn phím". Đây là nền P12 dùng lại.
Fix: listbox `tabIndex=0` + `aria-activedescendant` (id option ổn định theo index). ArrowUp/Down/Home/End/PageUp/PageDown đổi active index và gọi `virt.scrollToIndex`. Space toggle, Enter chọn. Checkbox để `tabIndex=-1`, `aria-hidden` vì trạng thái đã thể hiện qua `aria-checked`/`aria-selected` của option. Lý do thêm: control tương tác lồng trong `option` vi phạm ARIA.

## MODERATE

- **M1. lint:safety dễ lách** (`scripts/lint-safety.mjs`). Probe 26 dòng: script bắt 5 mẫu, Biome bắt thêm `window.eval`/`globalThis.eval`/`javascript:` href. **Lọt:** `el.setHTMLUnsafe()`, `range.createContextualFragment()`, `Reflect.set(el,"innerHTML")`, `el[k]=` với key động, `Function("…")` không có `new`, `setInterval("…")`, `<iframe` xuống dòng ngay sau tên thẻ, `document.createElement("iframe")`, `srcdoc`, và **code đứng sau comment trên cùng dòng** (dòng 39 bỏ qua mọi dòng mở đầu bằng `/*`). Fix: thêm regex cho `setHTMLUnsafe|createContextualFragment|srcdoc|\bFunction\s*\(|set(Timeout|Interval)\s*\(\s*["'\`]|createElement\(\s*["'\`](iframe|object|embed)`. So `<(iframe|object|embed)\b` thay vì `[\s>]`. Chỉ strip phần comment thay vì bỏ cả dòng. Về lâu dài nên chuyển sang Biome GritQL plugin hoặc rule AST. (CSP `script-src 'self'` đã chặn eval/Function lúc runtime, nên lỗ thật nằm ở họ innerHTML.) Phụ: trên Windows `SKIP` dùng `"ipc/bindings.ts"` nên không khớp đường dẫn có `\`. Hiện vô hại, nhưng nên normalize.
- **M2. Mapping shadcn (§2.3) là CSS chết và có hai hàng không thể hiện được** (`tokens.css:86-101`). Các alias `--primary/--background/--ring…` không được đưa vào `@theme` và không component nào dùng. `--muted` (nền → surface-2) và `--accent` (hover → sel) trùng tên với token OpenSVN nên bị bỏ âm thầm. Nếu sau này vendor component shadcn, `bg-accent` sẽ ra xanh accent thay vì `--sel`. Fix: hoặc bỏ block (YAGNI, vì không dùng shadcn CLI), hoặc giữ lại kèm comment + quy tắc "khi vendor shadcn: `bg-accent`→`bg-sel`, `bg-muted`→`bg-surface-2`" và báo PO ghi chú vào §2.3.
- **M3. Token mới `--on-accent`, `--overlay` chưa có trong §2.1** (`tokens.css:27-28`). Giá trị lấy từ §2.2/§6 nên hợp lệ, nhưng design-guidelines yêu cầu sửa doc + §10. Không nhất quán: `button.tsx:9` danger và `status-glyph.tsx:11` C dùng `text-white` cứng thay vì token. Gộp xử lý cùng mục contrast bên dưới.
- **M4. Popover trigger bọc `<span className="contents">`** (`app-shell.tsx:44`). `aria-expanded/haspopup` gắn vào span, không vào button. Khi đóng, Radix trả focus về span (không focus được) nên focus rơi về `body`. Fix: truyền thẳng `ToolbarItem` làm child `asChild` (React 19 truyền `ref` như prop, `...rest` đã forward).
- **M5. Tabs không render `Tabs.Content`** (`tabs.tsx:27`). Radix vẫn gắn `aria-controls` vào id không tồn tại. axe sẽ báo, và plan đặt tiêu chí "axe 0 serious". Fix: render `T.Content` cho từng tab, hoặc bọc nội dung caller trong `T.Content`.
- **M6. ModalDialog gọi `onClose` 2 lần khi bấm Esc** (`modal-dialog.tsx:34-41`: `cancel` + `keydown`). Không có handler cho event `close` native (Chromium CloseWatcher có thể đóng dialog mà không phát cancel cancelable), nên React state có thể lệch với `el.open`. Fix: bỏ `onKeyDown`, thêm `onClose={() => open && onClose()}`.
- **M7. Font OFL-1.1 bundle vào app** cần kèm license/copyright notice khi phân phối. Thêm vào third-party notices (P14) hoặc copy `LICENSE` của fontsource.
- **M8. Deps**: `@types/node` dùng `^24.19.2` (`package.json:40`), không pin chính xác như các dep khác. `tailwindcss`/`@tailwindcss/vite` là dep build-time nhưng nằm trong `dependencies`, kéo lightningcss (MPL) vào danh sách "prod" của `pnpm licenses`/audit. Nên chuyển sang `devDependencies`. `@codemirror/*` hiện chỉ spike dùng: chấp nhận được vì DiffView (bước 6) là scope của P09.
- **M9. Map `ops` trong cache tăng không giới hạn** (`events.ts:168-177`). Không bao giờ dọn op đã xong. Nên giữ N op cuối hoặc TTL. Nhỏ, nhưng là state sống suốt app.

## Contrast (WCAG 2.x, tính lại bằng công thức relative luminance)
Lời dev **đúng**: chữ trắng trên `--accent` dark = **2.62:1**. Nhưng còn fail thêm:

| Cặp | Light | Dark | Dùng ở |
|---|---|---|---|
| #fff trên `--accent` | 5.56 ✓ | **2.62 ✗** | Button primary, badge ↓N, hover menu item |
| #fff trên `--bad` | 5.30 ✓ | **2.91 ✗** | Button danger (§2.2 bắt chữ trắng) |
| #fff trên `--warn` | **4.24 ✗** | **2.19 ✗** | Glyph C |
| `--warn` trên `--surface` | **4.24 ✗** | 7.75 ✓ | Glyph M 12px |
| `--ok` trên `--surface` | **4.38 ✗** | 7.53 ✓ | Glyph A, "Connected" |
| `--warn` trên `--warn-soft` | **3.88 ✗** | 6.14 ✓ | Tiêu đề banner (13px bold không tính là "large") |
| `--muted`/`--accent` trên `--sel` | 4.35 / 4.46 ✗ | 4.59 ✓ | Meta và "↓ chưa update" trên hàng đang chọn |
| `--bad` trên `--sel` | 4.25 ✗ | 4.14 ✗ | Lock người khác trên hàng đang chọn |

Phương án tối thiểu cho PO (không đổi tên token, ưu tiên không đổi hex v1):
- **A (khuyến nghị, không đổi hex nào):** thêm token chữ-trên-nền theo theme `--on-accent`, `--on-bad`, `--on-warn`. Light = `#fff` (riêng `--on-warn` xem B). Dark = `#0f141b` (= `--bg` dark), cho ra 7.05 trên accent, 6.35 trên bad, 8.45 trên warn. Chỉ sửa câu §2.2 "Chữ trên --accent/--bad là #fff" thành "… #fff ở light, --bg ở dark".
- **B (đổi nhẹ 2 hex light):** `--warn` light `#b26a00`→`#a36100` (4.92 trên warn-soft, ≥4.5 trên surface, chữ trắng ≥4.5). `--ok` light `#1f8a4c`→`#1e864a` (4.6). Mắt thường gần như không thấy khác.
- **C (đổi ở mức component):** glyph C dùng chữ `--fg` trên `--warn-soft` + viền `--warn`, thay vì chữ trắng trên `--warn`. Hàng đang chọn giữ màu chữ nhưng `--sel` cần tối/nhạt hơn. Phương án này tốn công hơn, không khuyến nghị cho v1.
- Cặp trên `--sel` (light) chỉ thiếu 0.05–0.25. Có thể chấp nhận như ngoại lệ cho text phụ trên hàng chọn, hoặc chỉnh `--sel` light nhẹ. PO quyết.

## Perf spike (ADR-002)
Phương pháp: chấp nhận được làm proxy. Scroll bằng script trong rAF, có baseline plain-div trên cùng máy, chạy bản release, số liệu tất định. Hạn chế:
1. Gán `scrollTop` mỗi frame là main-thread scroll. Cách này đo được việc React/virtualizer có theo kịp không, nhưng không đo được checkerboarding/hàng trắng khi scroll async trên compositor (đó mới là thao tác thật của người dùng). Nên ghi thêm số frame mà `getVirtualItems()` không phủ hết viewport.
2. Làm tròn ms của WebKit: ngưỡng ">20ms" không bị ảnh hưởng (frame 16.7ms chỉ đo ra 16/17ms). Nhưng avg 57.6 fps cùng lúc với 21% frame >20ms nghĩa là rAF jitter (các frame 21ms/12ms xen kẽ) do máy tải nặng, chưa chắc là rớt frame thật. p95 = 32ms nghĩa là khoảng 5% frame rớt 1 vsync.
3. Đo khi máy **đang tải nặng** (~9 agent compile), chỉ 1 run cuối, baseline cũng có 16% frame chậm. Plan yêu cầu median 3 run, nhưng mới có 1. Màn 60 Hz, chưa thử ProMotion. Không có số RAM trên macOS (plan yêu cầu fps/RAM, đề xuất RSS của WebContent).
4. Spike chạy dưới `devCsp` (`'unsafe-inline'`), dev đã ghi chú. CodeMirror dưới CSP production chưa kiểm.

Kết luận: số macOS **đủ** để nói "macOS không phải blocker". Diff open 101–184 ms so với ngưỡng 300, list 57.6 fps so với ngưỡng chuyển Electron là <50 fps (ADR-002 dòng 60). Nhưng **chưa đủ** để tick tiêu chí "60 fps": cần chạy lại 3 lần trên máy rảnh, lấy median, ghi RSS. ADR-002 chỉ đóng được khi có Linux (Intel/AMD/NVIDIA) + Windows. Không chặn merge PR vì việc này.

## File ngoài owner_paths
`biome.json` (root, 1 dòng `css.parser.tailwindDirectives`, cần để parse `@theme`), `scripts/lint-safety.mjs`, `perf-tauri.conf.json`, `src/perf/`, `src/gallery/`, `src/test-setup.ts`, `src/lint-safety.test.ts`. Cũng đã xoá `src/App.tsx`/`App.test.tsx` của 01b. Tất cả là tooling/dev-only, phạm vi hẹp, chấp nhận được, chỉ cần lead ack `biome.json`. Lưu ý: `perf:spike` build ra cùng `dist/` với production. Nên dùng `--outDir dist-perf` để không bao giờ ship nhầm bundle có `/__perf` nếu chạy `tauri build --no-before-build`.

## Plan status (phase-09)
Đã xong cho M1: tokens/fonts, component §6 (Dialog/Sheet dùng `<dialog>` native), layout + resizable sidebar, IPC client/queries/event bus/mock (`dev:mock`), lint:safety + XSS test, `/__perf`, `/__gallery`.
Chưa có (đúng scope M1, ghi lại để theo dõi): DiffView/ImageCompare, i18n (vi mặc định), phím tắt theo OS, test axe, spike Linux/Windows, SafeLink opener phía Tauri. UI hiện toàn chuỗi tiếng Anh, trong khi §9 quy định tiếng Việt mặc định. Việc này phụ thuộc bước i18n.

## Unresolved questions
1. PO chọn phương án contrast nào (A / A+B / khác)? Có chấp nhận ngoại lệ cho text phụ trên `--sel` light không?
2. Mở link ngoài: dùng `tauri-plugin-opener` (dep Rust mới, P08) hay command riêng của P08? Ai sở hữu?
3. Giữ block alias shadcn (§2.3) hay bỏ, vì không dùng shadcn CLI?
4. Chuỗi UI tiếng Anh tạm thời trong M1 có được chấp nhận cho tới bước i18n không?

Status: DONE
Summary: REQUEST CHANGES. Có 5 lỗi IMPORTANT: event bus double-dispatch/leak (đã chứng minh bằng test), mock adapter trong bundle production + fallback, lỗi string không map sang AppError, SafeLink không có opener cho Tauri, listbox không dùng được bằng bàn phím. Các gate (biome/tsc/vitest/build/CI 3 OS) đều xanh, token khớp v1, dev route không có trong dist, CSP và font đều ổn. Contrast dark của accent/bad/warn fail như dev báo, kèm vài cặp light. Đã đề xuất phương án A (token on-* theo theme) cho PO.

---

## Re-review (2026-10-13, HEAD `ceb2a82`, chạy local vì CI bị chặn billing)

### Verdict: APPROVE (kèm 2 follow-up, xem F1/F2)

Gates local trên `ceb2a82`: `biome check` OK (80 file) · `tsc` OK · `vitest` 52/52 (11 file) · `pnpm build` + `check-dist` OK. Không cần cargo (không có thay đổi Rust trong PR). Đã dọn `dist/` và các file probe sau khi chạy.

| Mục | Kết quả | Bằng chứng |
|---|---|---|
| I1 event bus | **Fixed** | `events.ts`: thêm `generation` + `starting`. Chạy lại test race của tôi: start→stop→start gọi handler 1 lần (trước đây 2). Unmount trong lúc start đang chạy: 0 listener rò. `stop()` sau đó vẫn gỡ hết. |
| I2 mock trong dist | **Fixed** | `initIpc()` dùng dynamic import sau `MOCK_ALLOWED` (fold thành false ở release). Bundle không còn `mock-op`/`not implemented`. Probe `check-dist`: thả file chứa `mock-op-` vào dist thì exit 1. Script nằm trong `pnpm build` nên CI sẽ chạy. Release thiếu bridge thì hiện màn `FatalError`, không dùng dữ liệu giả. |
| I3 AppError | **Fixed** | `toAppError`/`unwrap`: đã probe reject string, `{status:"error", error:"boom"}`, `TypeError`, `{}`, `undefined`. Tất cả cho ra `code:"internal"` (có trong `ErrorCode` của bindings) với message không rỗng, tên lỗi `IpcError`. |
| I4 opener | **Fixed phía FE, thiếu phía Rust** (F1) | Release bundle: 0 `window.open`/`_blank`/`noopener`. `window.open` chỉ chạy khi `MOCK_ALLOWED`. Thiếu opener thì throw. Có `onAuxClick` + `draggable={false}`. |
| I5 listbox | **Fixed** | Listbox là tab stop duy nhất, dùng `aria-activedescendant`, đủ Up/Down/Home/End/PgUp/PgDn (gọi `scrollToIndex` tới cả hàng chưa mount), Enter/Space. Checkbox `tabIndex=-1 aria-hidden`, option có `aria-checked`. ChangesSidebar đã nối `onActivate/onToggle`. `keyboard.test.tsx` kiểm hành vi thật, không phải test hình thức. |
| M2/M3/M5/M6/M7/M8/M9, M4 | **Fixed** | Bỏ alias shadcn, thay bằng comment dịch class (+ ghi vào doc §2.3). Thêm `--overlay`/`--on-*` vào doc. Tabs render `T.Content` với `forceMount` nên `aria-controls` luôn trỏ tới element có thật. ModalDialog bỏ `onKeyDown`, thêm `onClose` native qua `openRef`. File OFL nằm ở `public/licenses` và có trong `dist`. Deps pin chính xác, tailwind chuyển sang dev. `MAX_OPS=50`. Popover trigger truyền `ToolbarItem` trực tiếp. Bản perf build ra `node_modules/.cache/perf-dist`. |
| M1 lint:safety | **Phần lớn fixed, có regression mới** (F2) | Probe 24 dòng, bắt được: `["innerHTML"]`, `Reflect.set(…"innerHTML")`, `setHTMLUnsafe`, `Function("…")`, `(0,eval)`, `writeln`, `setInterval("…")`, `createContextualFragment`, `<iframe` xuống dòng, `createElement("iframe")`, `srcdoc`, code sau `/* */`, `insertAdjacentHTML`. Lọt: `const F = Function; F(…)`, `el[k]` key động, `(()=>{}).constructor("…")` (lách có chủ đích, không phải lỗi vô ý, chấp nhận vì CSP là lớp chặn cuối), `window.eval` (Biome bắt được). |

### F1 [IMPORTANT, follow-up cho P08, không chặn PR này]: chưa có `tauri-plugin-opener` phía Rust
`apps/opensvn/src-tauri` không có crate `tauri-plugin-opener`, không có `.plugin(tauri_plugin_opener::init())`, và `capabilities/default.json` không có `opener:*`. Vì vậy `openUrl()` ở release sẽ reject: fail-closed (SafeLink log lỗi), không mở được link. Comment `src/lib/external-link.ts:4-5` ("the Rust side scopes it to https") **chưa đúng**, cần sửa comment ngay trong PR này hoặc ghi TODO. Hiện SafeLink chưa dùng trong production (bị tree-shake) nên chưa ai thấy lỗi. Việc cho P08 (`src-tauri` là owner path của P08):
- Thêm crate `tauri-plugin-opener` cùng minor với JS `2.7.1` và `init()` trong builder.
- Thêm capability `{"identifier":"opener:allow-open-url","allow":[{"url":"https://*"}]}`. **Không** dùng `opener:default` vì nó cho phép cả http/mailto/tel.
- Không cấp `allow-open-path`/`allow-reveal-item-in-dir` (đã có command `reveal_in_os` riêng).

### F2 [MODERATE]: `stripComments` không hiểu string literal nên một số chuỗi tắt quét
`scripts/lint-safety.mjs:31-48`. Probe:
- `const glob = "src/*";` → vào trạng thái block comment, **các dòng sau không bị quét** cho tới khi gặp `*/`. Ví dụ `el.innerHTML = …` ở dòng kế tiếp bị lọt.
- `const u = "a//b"; el.innerHTML = z;` → phần sau `//` bị cắt, lọt.

Đây là trường hợp dễ gặp thật: `accept="image/*"` (ImageCompare), `"text/*"`, glob. Script cũ không có lỗi này vì chỉ bỏ dòng *bắt đầu* bằng comment. Fix: dùng `ts.createScanner` của TypeScript (đã có trong devDeps) để lấy token comment chính xác, hoặc cho `stripComments` bỏ qua nội dung trong `'…'`/`"…"`/`` `…` ``. Thêm 2 case trên vào `lint-safety-rules.test.ts`.

### Contrast: tôi tự tính lại từ `tokens.css`
Tôi parse 3 block (`:root`, `@media … :root:not(...)`, `[data-theme=dark]`). Hai block dark **giống hệt nhau**, nhưng `contrast.test.ts` chỉ đọc `:root` và `[data-theme="dark"]`, không đọc block media. Nên thêm một assert rằng hai block dark bằng nhau, để chặn drift về sau.
- Fill: on-accent 5.56 / 7.05 · on-bad 5.30 / 6.35 · on-warn 4.95 / 8.45 (light/dark). Ở trạng thái hover `brightness-110` (tính trong linearRGB): light on-accent 5.16, on-bad 4.94, vẫn ≥4.5.
- Toàn bộ 7 token chữ (`fg` + 6 `*-text`) × 9 nền (`surface, surface-2, sel, bg, accent-soft, warn-soft, bad-soft, add-bg, del-bg`) × 2 theme: **0 cặp dưới 4.5**. Số liệu khớp bảng v1.1 trong design-guidelines.
- Grep code app: không còn component nào dùng token gốc (`text-muted/accent/ok/warn/bad/info`) làm màu chữ (trừ trang perf dev-only). Hex v1 không đổi.

**`#000` trên `--warn` (light):** đạt 4.95:1 (ngưỡng 4.5). Với `--warn` #b26a00 giữ nguyên, đây là màu trung tính **duy nhất** đạt chuẩn: `#fff` 4.24, `--fg` #1b2533 3.65, `#0f141b` 4.36. Nên tôi **chấp nhận** cả về guideline lẫn thị giác:
- Về guideline: đây là token (`--on-warn`), không phải mã màu cứng, và đạt AA. §2.2 v1.1 đã ghi rõ.
- Về thị giác: chữ đen trên nền hổ phách là quy ước cảnh báo quen thuộc. Phạm vi chỉ là glyph "C" 12px/500 trong ô 18×20.

Lưu ý cho PO: (a) margin mỏng (4.95), nên nếu sau này chỉnh `--warn` light thì phải chạy lại test; test đã có. (b) Ở light, glyph C có cực tính ngược với primary/danger (tối trên sáng vs sáng trên tối). Đây là chủ ý và chấp nhận được. (c) Dòng §10 "PO chọn phương án A … on-warn light = đen" do dev ghi: **lead cần xác nhận PO thật sự đã duyệt giá trị `#000`**, vì phương án A gốc trong báo cáo của tôi để ngỏ `--on-warn` light.

### Minor (không cần sửa)
- Nếu `adapter.listen` reject giữa chừng, `starting` giữ promise lỗi đó cho tới `stop()`, nên các lần `start()` sau trả lại đúng lỗi cũ. Hiện chấp nhận được vì Providers log rồi remount.
- `aria-activedescendant` có thể trỏ tới hàng đã bị unmount khi người dùng cuộn chuột ra xa hàng active. Hiếm gặp, có thể xử lý ở P12 (scroll tới active khi focus lại).
- Eviction `MAX_OPS` theo thứ tự chèn có thể đẩy ra một op đang chạy lâu nếu có >50 op mới hơn. Không thực tế ở M1.

### Unresolved
1. PO đã duyệt `--on-warn: #000` (light) chưa?
2. P08 nhận F1 (opener Rust + capability https-only) vào PR nào?

Status: DONE
Summary: Re-review APPROVE trên `ceb2a82`. 5 IMPORTANT và các MODERATE đều đã sửa, xác minh bằng probe và test local. Còn 2 follow-up: F1 phía Rust chưa có plugin-opener (việc của P08, comment FE đang mô tả sai) và F2 `stripComments` bị string chứa `/*` hoặc `//` làm tắt quét. Contrast 0 cặp fail, `#000` trên warn chấp nhận được (4.95, là màu trung tính duy nhất đạt chuẩn).
