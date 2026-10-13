# Review PR #53: 12a FE remote screens M1

Ngày: 2026-10-13 · Reviewer: code-reviewer · HEAD `aef7c1c` (2 commit trên main, merge-tree với origin/main sạch) · worktree `OpenSVN-worktrees/review-pr53` · chạy local (CI skip)

## Verdict: REQUEST CHANGES

Chỉ có 1 lỗi IMPORTANT chặn merge: nút SSO gửi variant `{kind:"negotiate"}`, nhưng Rust không có variant này nên operation bị treo. Sửa nhỏ. Quy tắc TLS, cách render text và SafeLink đều đúng spec. Gates xanh.

## Gates
| Gate | Kết quả |
|---|---|
| `pnpm install --frozen-lockfile` | OK |
| `biome check .` | OK, 132 file |
| `tsc --noEmit` | OK |
| `vitest run` | 136/136 (21 file) |
| `lint:safety` | ok |
| `pnpm build` + `check-dist` | OK. Bundle không có `mock-op`/`remote-data`/`window.open`. Chuỗi "Locked by an" khớp grep là false positive (của `Locked by another user` trong LockBadge). |

## Bảo mật: đã xác minh đạt
- **TLS** (`auth/auth-model.ts:122-125`, `trust-cert-dialog.tsx`):
  - `isTrustable` chỉ đúng với `unknown-issuer | self-signed`; chỉ khi đó mới có nút Từ chối · Tin tạm thời · Tin vĩnh viễn, khớp doc 01 §4.4.
  - `hostname-mismatch | expired | not-yet-valid | revoked | {other}` chỉ có nút Đóng, kèm hướng dẫn liên hệ IT.
  - Chuỗi lạ, object lạ đều về `{other}` nên bị chặn (fail-closed).
  - `changed` có tiêu đề cảnh báo riêng, hiện vân tay cũ và mới. Không có trust one-click: nút danger "Thay pin" chỉ bật sau khi tick xác nhận.
  - Esc hoặc đóng dialog đều gửi `reject`. Focus mặc định rơi vào nút Từ chối/Đóng (nút đầu tiên trong footer).
  - Trường hợp cert vừa đổi vừa sai hostname: theo doc 03 backend phải báo `hostname-mismatch`, và UI chặn cứng trường hợp này. Test phủ đủ các nhánh (`auth.test.tsx:181-227`).
- **Không render HTML**: message, realm, subject/issuer, tên file, lock owner/comment, blame, diff, file preview đều là text node (`SafeText`/`<pre>`). Grep `features/` không có sink nào. Có test cho chuỗi độc hại.
- **Linkify** (`history/linkified-text.tsx`): regex chỉ bắt `https://`, link render qua `SafeLink`, dấu câu cuối được tách ra ngoài link. `expandWebUrl`/`isHttpsTemplate` chỉ cho ra URL https.
- **Credentials**: password chỉ nằm trong state của `SignInDialog` và mất khi dialog unmount (`respond` gỡ prompt khỏi queue trước khi gọi IPC). Không có `console.*` nào đụng tới. Toast chỉ hiện `AppError.message`. Phụ: `mockAuthResponses` giữ password trong mảng module, nhưng chỉ ở dev/mock và đã kiểm không có trong dist.
- **PathGrant**: không có ô nhập path. Checkout root và external tool ở Settings là `readOnly`, nút đổi bị disable. Download/Upload trong menu node bị disable. Có test "paths and commands are display-only".

## IMPORTANT

### I1. SSO gửi `{kind:"negotiate"}` ngoài contract nên operation bị treo
`features/remote/auth/auth-prompt-host.tsx:11,37,58`
`AuthResponse` trong bindings (và trong `opensvn-ipc` trên main) **không có** variant `negotiate`. Cast `response as AuthResponse` làm tsc im lặng. Trong Tauri thật, serde từ chối tham số trước khi command chạy, nên invoke reject bằng string. Kết quả: hiện toast "invalid args…", prompt đã bị gỡ khỏi queue, **còn engine vẫn chờ `auth_respond` mãi** và operation treo. Mock adapter nhận bất kỳ thứ gì nên test không bắt được.
Fix, chọn một:
- (a) Ẩn nút SSO cho tới khi contract có variant: thêm `const SSO_SUPPORTED = false` kèm comment, hoặc kiểm bằng type `Extract<AuthResponse,{kind:"negotiate"}>`. Đây là cách khuyến nghị.
- (b) Nếu vẫn giữ: khi `authRespond` lỗi thì gửi bù `{kind:"cancel"}` cho cùng `requestId` để engine không treo.

Nên làm cả (b) cho mọi lỗi của `respond`, không riêng SSO, vì đó là lưới an toàn chung. Đồng thời mở contract request cho P01/P08: thêm `{ kind: "negotiate" }` vào `AuthResponse`.

## MODERATE

- **M1. Phân tích payload auth hai dạng là cầu tạm hợp lệ, nhưng chưa có cơ chế buộc phải gỡ** (`auth-model.ts:61-117`, `as unknown as`).
  - Lý do cần: `opensvn-ipc` trên main vẫn có `cert.failure: String` và không có `reason/attempt/offered`. PR #43 chỉ sửa doc 03 và `svn-ra-api`, chưa regen bindings. Vì vậy hiện tại không bỏ được nhánh này.
  - Đề xuất: đặt `// @ts-expect-error -- remove once bindings carry reason/attempt/offered` ở chỗ đọc `p.reason`. Khi `gen:ipc` sinh field mới thì tsc fail, buộc phải xoá nhánh. Ghi kèm điều kiện gỡ và số issue.
- **M2. Backend phải tự kiểm `CertDecision`, UI không phải trust boundary** (follow-up cho P04/P08, không thuộc PR này). `auth_respond` cần từ chối `acceptOnce`/`acceptAlways` khi failure không thuộc `unknown-issuer|self-signed`, và chỉ chấp nhận `acceptAlways` cho `changed` như một lần thay pin. `cert-decision.ts` map `replacePin` thành `acceptAlways`, nên backend không phân biệt được "thay pin có xác nhận" với một "luôn tin" thường. Đề xuất contract thêm `CertDecision = "replacePin"` để backend enforce và ghi audit.
- **M3. Tạo branch/tag đoán layout bằng regex, lệch với popover** (`branch/branch-model.ts:3-13`, `create-branch-tag-dialog.tsx:53`).
  - Popover chọn chế độ theo `wc.branch` (backend biết cả layout người dùng tự cấu hình, §4.11), còn dialog chọn theo regex `trunk|branches|tags` trên URL.
  - Hậu quả: repo có layout tự cấu hình (vd `/Dev/Main`) hiện nút "New branch…" nhưng dialog lại báo `cannotDerive`.
  - Fix: dialog lấy project root từ `layoutGet`/`branchesList` (đã có trong bindings), regex chỉ làm fallback.
- **M4. Tabs Views trong app-shell không dùng `panels`** (`app/app-shell.tsx`). Nội dung render ngoài `tabpanel`, nên `aria-controls` trỏ tới panel rỗng. Chính lỗi M5 của P09 vừa sửa đã quay lại ở đây. Fix: truyền `panels={{changes:…, history:…, browse:…}}`. Nhãn "Changes/History/Browse" cũng còn hardcode tiếng Anh, chưa qua i18n.
- **M5. Ngữ nghĩa phân trang log chưa có trong contract** (`history/use-history.ts:59`).
  - FE gửi `fromRev = nextRev`, tức là inclusive. Doc 03 chỉ ghi `nextRev?: Rev`, không nói nextRev là rev đầu tiên của trang sau hay rev cuối của trang hiện tại.
  - Nếu backend trả rev cuối của trang hiện tại thì trang sau sẽ trùng 1 entry.
  - Đề xuất: doc 03 ghi rõ "nextRev = fromRev cho trang kế (≤ rev cuối − 1)", và FE dedupe theo `rev` khi `flatMap` (rẻ).
  - Còn lại đúng: queryKey gồm filter text và stopOnCopy; filter author/ngày chạy client-side trên các trang đã tải; có nút Load more và prefetch khi di chuyển bằng bàn phím.
- **M6. Mỗi `op://finished` invalidate `repoRoot` + `logRoot`** (`ipc/events.ts`, shared). Cả op chỉ đọc như blame cũng kích hoạt refetch **mọi trang** đã tải của infinite log và mọi listing trong tree đang mở. Fix: chỉ invalidate với `OperationKind` làm thay đổi server (`remoteEdit|copyLocation|lock|unlock|commit…`).
- **M7. Repo tree: hàng active không được cuộn vào tầm nhìn khi đi bằng bàn phím** (`browse/repo-tree.tsx:100-130`). Fix: `document.getElementById(id)?.scrollIntoView({block:"nearest"})` sau khi đổi `active`. Thêm nữa, `active` lưu theo index nên sau khi expand hoặc data đổi có thể trỏ sang node khác. Nên lưu theo URL.
- **M8. Settings: "chỉ gửi key đã đổi" chưa hoàn toàn đúng** (`settings-sheet.tsx:69`). Draft giữ key kể cả khi người dùng sửa rồi đổi lại đúng giá trị cũ. Khi đó Save vẫn bật và patch gửi kèm giá trị không đổi. Vô hại về dữ liệu (plain key: absent = giữ nguyên, có giá trị = set). Phía FE chưa bao giờ gửi `null` (đúng, vì chỉ `diff.externalTool` nullable và đang readOnly). Fix: trong `set`, xoá key khỏi draft khi giá trị bằng `settings[k]` (deep-equal cho `network.proxy`).

## Nhỏ
- `crumbs()` (`repo-url.ts:37`) gọi `decodeURIComponent` không try/catch. Một segment percent-encoding hỏng từ server sẽ làm crash render. `baseName` đã có try, nên làm tương tự.
- `Tabs.keepMounted` được thêm vào file shared nhưng không ai dùng (YAGNI), nên bỏ.
- `AuthPromptHost` unmount khi còn prompt trong queue thì không trả lời các prompt đó. Hiếm, vì host sống suốt app.

## Câu hỏi của dev: cert đổi chỉ cho "Từ chối" hoặc "Thay pin có xác nhận", không có tin tạm thời. Đồng ý không?
**Đồng ý.**
- Một nút `acceptOnce` cho `changed` tương đương chấp nhận một MITM tiềm năng trong cả phiên chỉ bằng một click, kèm theo credential gửi sau đó. Đây đúng là kịch bản mà việc pin sinh ra để chặn.
- Trường hợp hợp lệ (server thay cert) đã được phục vụ bằng thay pin, có bước đối chiếu vân tay.
- Phù hợp doc 03: "`changed` không bao giờ luôn tin cậy mà không có thao tác thay pin rõ ràng". Doc 01 §4.4 chỉ ghi "cảnh báo riêng", không bắt buộc có tin tạm thời.
- Đề nghị lead/PO ghi rõ danh sách nút của trường hợp `changed` vào doc 01 §4.4. Kèm M2: backend cần decision `replacePin` riêng.

## Keyboard / ARIA
- Đạt: listbox (history, blame, dir-table) dùng lại `VirtualList` (`aria-activedescendant`). Tree: `role=tree/treeitem` + `aria-level/expanded/selected`, đủ Up/Down/Home/End/Left/Right/Enter. Dialog/Sheet dùng `<dialog>` native nên focus trap và Esc có sẵn. Menu node vừa có ở chuột phải vừa có ở dropdown "Actions" (đi tới được bằng bàn phím). File preview là vùng cuộn focus được. Settings nav có `aria-current`. Biome bật `noLabelWithoutControl` cho `Input/Select/Textarea`.
- Chưa đạt: M4 (tabpanel), M7 (scroll của tree).

## File shared của P09 và `biome.json`
Chấp nhận được, đều là thay đổi cộng thêm:
- `virtual-list` thêm `onActiveChange`.
- `tabs` thêm `keepMounted` (bỏ, xem mục Nhỏ).
- `providers` thêm `BusContext` + `SettingsI18n`.
- `events` thêm `opOutcomes`, có giới hạn `MAX_OPS`. Đây là cách sửa đúng cho race finished-trước-started. Riêng invalidate xem M6.
- `queries`/`query-keys` thêm hook và key mới, `app-toolbar` thêm `onOpenSettings`.
- `biome.json` siết rule a11y. Cần lead ack, không có rủi ro.

## Plan follow-ups (12a)
Đã có: auth/cert dialog, Browse (tree + bảng + preview + remote edit), History (filter, phân trang, chi tiết, diff), Blame, Lock/Unlock/Steal, Branch/Tag/Copy, Settings (7 trang), i18n vi/en, gallery demo.
Còn chờ: SSO (contract), Download/Upload và chọn thư mục checkout (PathGrant), danh sách cert đã tin trong Accounts (đang hiện note), regen bindings sau #43.

## Unresolved questions
1. Ai thêm `{kind:"negotiate"}` và `CertDecision "replacePin"` vào `opensvn-ipc`, và khi nào regen bindings sau #43?
2. Doc 03: chốt ngữ nghĩa `nextRev` (inclusive cho trang kế)?
3. Doc 01 §4.4: lead/PO ghi chính thức các nút cho trường hợp "cert đã đổi"?

Status: DONE
Summary: REQUEST CHANGES vì 1 lỗi IMPORTANT: SSO gửi `{kind:"negotiate"}` ngoài contract nên engine treo chờ `auth_respond`. TLS dialog, render text, SafeLink https-only, credential và PathGrant đều đạt; gates xanh. Có 8 mục MODERATE: cầu parse auth tạm chưa có cơ chế buộc gỡ, backend chưa kiểm decision, layout đoán bằng regex, tabpanel rỗng, chưa chốt ngữ nghĩa nextRev, invalidate thừa, scroll của tree, draft Settings. Tôi đồng ý với quyết định không cho tin tạm thời khi cert đã đổi.

---

## Re-review (2026-10-13, HEAD `6e7bf69`)

### Verdict: REQUEST CHANGES (1 lỗi mới do bản sửa M3 gây ra, sửa nhỏ)

Gates local: biome OK (133 file) · tsc OK · vitest 141/141 · lint:safety ok · build + check-dist ok. Đã dọn `dist/` và file probe.

| Mục | Kết quả |
|---|---|
| I1 SSO | **Fixed**. Đã bỏ nút SSO và cast `negotiate`. `answerAuthPrompt` gửi bù `cancel` khi gặp bất kỳ lỗi respond nào (trừ khi chính lệnh bị lỗi đã là cancel). Mock từ chối variant lạ giống serde. Có test `auth.test.tsx:263` kiểm chuỗi negotiate → lỗi → cancel. |
| M1 | **Fixed**. Ba `@ts-expect-error` cho `reason/attempt/offered` (`auth-model.ts:102-107`): tsc sẽ fail khi `gen:ipc` sinh field mới. |
| M3 | **Fixed nhưng có lỗi mới** (R1). Thứ tự đúng: `branches_list` → `layout_get` (custom/none) → quy ước. Nút tạo bị khoá trong lúc lookup. |
| M4 | **Fixed**. Tabs dùng `panels` + `keepMounted` (lead đã chấp nhận). Nhãn tab đã qua `shellStrings`. |
| M6 | **Fixed**. Chỉ refetch với `commit/lock/unlock/remote-edit/switch/merge`. `copy_location` chạy dưới kind `remote-edit` nên vẫn refetch. Op chưa từng thấy `started` thì mặc định refetch (an toàn). |
| M7 | **Fixed**. Active lưu theo URL, có `scrollIntoView({block:"nearest"})`. |
| M8 | **Fixed**. `withoutUnchanged` so sánh deep bằng JSON để bỏ key đã đổi về giá trị cũ. Có test. |
| Doc 01 §4.4 | **Fixed**. Đã có bảng nút theo loại lỗi, khớp code. Có ghi rõ trường hợp vừa đổi vừa sai hostname được báo là hostname. |
| `nextRev` | **Fixed**. Dedupe theo `rev`. Comment ghi semantics inclusive. Doc 03 vẫn nên ghi câu này (không chặn). |
| M2 | Chờ PR contract của dev-01a, không chặn. |

### R1 [IMPORTANT]: repo nhiều project sẽ tạo branch nhầm sang project khác
`features/remote/branch/branch-model.ts` (`refContainerUrl`): `ctx.nodes.find((n) => n.kind === wanted)` lấy branch/tag **đầu tiên của cả repo**, vì `branches_list(repoRootUrl)` trả về mọi project. Tôi đã chứng minh bằng probe: WC ở `/ProjectA/trunk`, nodes chứa `/ProjectB/branches/x`, thì kết quả là `/ProjectB/branches` (mong đợi `/ProjectA/branches`). `copy_location` commit thẳng lên server. Preview đường đích hiện trong dialog nên người dùng còn cơ hội thấy, nhưng doc 01 §3.9 hỗ trợ rõ dạng `/ProjectA/trunk`.
Fix: chỉ dùng node cùng project với `currentRefUrl ?? sourceUrl`. Ví dụ: so `parentUrl(parentUrl(node.url))` (với branches/tags) với `projectRootOf(current)`, hoặc so tiền tố project root. Nếu không có node khớp thì đi tiếp sang layout/quy ước. Thêm test 2 project.

Status: DONE
Summary: Re-review `6e7bf69`: I1, M1, M3–M8, doc §4.4 và dedupe nextRev đều đã sửa, gates xanh. Vẫn REQUEST CHANGES vì R1: `refContainerUrl` lấy branch node đầu tiên của cả repo nên repo nhiều project tạo branch sang project khác (đã chứng minh bằng probe). Sửa nhỏ, chỉ cần lọc node theo project.
