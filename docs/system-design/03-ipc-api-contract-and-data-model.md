# 03 — Hợp đồng IPC (UI ↔ Rust) & mô hình dữ liệu

Cập nhật: 2026-10-13 · Liên quan: [01](01-ui-screens-menus-and-interactions.md), [02](02-architecture-data-flow-and-reliability.md)

Type TS bên dưới là **nguồn tham chiếu thiết kế**; khi code, type sinh tự động từ Rust bằng `tauri-specta` (Rust là nguồn sự thật).

## 1. Quy ước
- Command đồng bộ ngắn (< 100ms, chỉ đọc cache/DB): trả kết quả trực tiếp.
- Command dài (mạng/đĩa): trả ngay `OperationHandle`, tiến trình & kết quả qua event `op://*`.
- Mọi lỗi theo `AppError` (§4). Không throw string.
- ID: `WcId`, `OpId` là UUID v7 (string). Đường dẫn trong WC luôn là **relpath** dùng `/`.
- Thời gian: ISO-8601 UTC string. Revision: `number` (u64 an toàn trong JS < 2^53).

## 2. Kiểu dữ liệu chính
```ts
type WcId = string; type OpId = string; type Rev = number;

interface WorkingCopy {
  id: WcId; name: string; rootPath: string;          // đường dẫn tuyệt đối OS
  repoRootUrl: string; repoUuid: string; url: string; // URL của gốc WC
  baseRev: Rev; revRange?: [Rev, Rev];                 // có khi mixed-revision
  format: 31 | 32; storePristine: boolean;
  branch: BranchRef | null;                            // null khi repo không có layout → UI dùng chế độ Location
  pathInRepo: string;                                  // vd "/2. Sản xuất/2. Phát triển" (hiển thị khi không có layout)
  incoming: number;                                    // số revision mới chưa update
  lastCheckedAt?: string;
}

interface BranchRef { kind: 'trunk' | 'branch' | 'tag' | 'other'; name: string; url: string; }

type NodeStatus = 'normal' | 'modified' | 'added' | 'deleted' | 'replaced' | 'unversioned'
  | 'missing' | 'conflicted' | 'ignored' | 'obstructed' | 'external';

interface FileStatus {
  path: string; kind: 'file' | 'dir' | 'symlink';
  text: NodeStatus; props: 'none' | 'normal' | 'modified' | 'conflicted';
  treeConflict: boolean; movedFrom?: string; movedTo?: string;
  changelist?: string; lock?: LockInfo; needsLock: boolean;
  remote?: { outOfDate: boolean; remoteRev?: Rev; remoteLock?: LockInfo };
  binary: boolean; size?: number;
}

interface LockInfo { owner: string; comment?: string; createdAt: string; token?: string; mine: boolean; }

interface LogEntry {
  rev: Rev; author?: string; date: string; message: string;
  changedPaths?: ChangedPath[]; hasChildren?: boolean;    // merged revisions (P6)
}
interface ChangedPath { path: string; action: 'A' | 'M' | 'D' | 'R'; kind: 'file' | 'dir' | 'unknown';
  copyFrom?: { path: string; rev: Rev }; textMods?: boolean; propMods?: boolean; }

interface RepoEntry { name: string; kind: 'file' | 'dir'; size?: number; createdRev: Rev;
  lastAuthor?: string; date: string; lock?: LockInfo; }

type DiffTarget =
  | { kind: 'wc'; wcId: WcId; path: string }                      // base ↔ working
  | { kind: 'rev'; url: string; rev: Rev }                         // rev-1 ↔ rev
  | { kind: 'range'; url: string; from: Rev; to: Rev }
  | { kind: 'wcVsRev'; wcId: WcId; path: string; rev: Rev };

interface FileDiff {
  path: string; binary: boolean; mime?: string;
  left?: TextSide; right?: TextSide;                               // text: full text 2 bên (CodeMirror tự diff)
  hunks?: Hunk[];                                                  // khi file quá lớn: chỉ gửi hunks
  propChanges: { name: string; before?: string; after?: string }[];
  image?: { leftUrl?: string; rightUrl?: string };                 // asset:// URL tạm
  tooLarge?: boolean;                                              // > 5MB text → hỏi trước khi tải
  tooComplex?: boolean;                                            // vượt ngân sách diff hiển thị (svn_diff::DISPLAY_MAX_COST): bỏ left/right, UI mời mở `diff_external`, không thử lại
}
interface TextSide { content: string; encoding: string; eol: 'LF' | 'CRLF' | 'mixed'; label: string; }
interface Hunk { oldStart: number; oldLines: number; newStart: number; newLines: number; lines: string[]; }

interface Conflict { path: string; kind: 'text' | 'property' | 'tree';
  base?: string; mine?: string; theirs?: string;                   // đường dẫn file tạm
  treeReason?: 'edited' | 'deleted' | 'missing' | 'moved' | 'added' | 'replaced';
  treeAction?: 'edit' | 'delete' | 'add' | 'replace' | 'move'; }

interface OperationHandle { opId: OpId; kind: OperationKind; wcId?: WcId; title: string; }
type OperationKind = 'checkout' | 'update' | 'switch' | 'commit' | 'revert' | 'cleanup' | 'lock'
  | 'unlock' | 'status-remote' | 'log' | 'blame' | 'remote-edit' | 'set-depth' | 'upgrade' | 'merge'
  | 'status-refresh' | 'add' | 'delete' | 'move' | 'copy' | 'save';

interface Progress { opId: OpId; phase: string; current?: number; total?: number;
  bytesDone?: number; bytesTotal?: number; currentPath?: string; }

// Tham số dạng union: tagged enum theo `kind` (camelCase).
type LogTarget = { kind: 'url'; url: string } | { kind: 'wc'; wcId: WcId; path: string };
type LockTargets = { kind: 'wc'; wcId: WcId; paths: string[] } | { kind: 'url'; urls: string[] };

// SVN không có branch/tag thật: chỉ là thư mục theo quy ước. Layout có thể không tồn tại.
type Layout =
  | { mode: 'auto' }                                                     // tự phát hiện trunk/branches/tags
  | { mode: 'custom'; trunk: string; branches: string[]; tags: string[] } // pattern, vd "/{project}/trunk"
  | { mode: 'none' };                                                    // repo tổ chức theo thư mục, không có branch
```

## 3. Commands

### 3.1 Working copy registry
| Command | Input | Output |
|---|---|---|
| `wc_list` | — | `WorkingCopy[]` |
| `wc_add_existing` | `{ grant: PathGrantId }` (thư mục chọn qua `path_pick` hoặc kéo-thả) | `WorkingCopy` |
| `wc_remove` | `{ wcId }` (không xóa file) | `void` |
| `wc_rename` | `{ wcId, name }` | `void` |
| `wc_open` | `{ wcId }` → bắt đầu watcher + status nền | `WorkingCopy` |
| `wc_close` | `{ wcId }` | `void` |

### 3.2 Status & thay đổi local
| Command | Input | Output |
|---|---|---|
| `status_get` | `{ wcId, includeIgnored?, includeUnversioned? }` | `FileStatus[]` (từ cache, có thể cũ) |
| `status_refresh` | `{ wcId, paths? }` | `OperationHandle` |
| `status_remote` | `{ wcId }` | `OperationHandle` (cập nhật `remote` trong FileStatus + `incoming`) |
| `diff_get` | `{ target: DiffTarget, ignoreWhitespace?, forceLarge? }` | `FileDiff` |
| `diff_external` | `{ target: DiffTarget }` | `void`. Mở `diff.externalTool` trên hai bản sao chỉ-đọc của hai bên (thư mục riêng `<data>/external-diff/<id>/{left,right}/<tên file>`, tự xóa sau 24h). Chạy trực tiếp, không qua shell; trong `args`, `{left}`/`{right}` được thay bằng đường dẫn hai bản sao, nếu không có placeholder thì hai đường dẫn được nối vào cuối. Chưa cấu hình → `invalidArgument` + action `openSettings`; file nhị phân → `invalidArgument` |
| `changelist_set` | `{ wcId, paths, name \| null }` | `void` |
| `ignore_add` | `{ wcId, pattern, dir, scope: 'svn:ignore' \| 'local' }` | `void` |
| `mark_move` | `{ wcId, from, to }` | `void` |

### 3.3 Thao tác ghi trên WC (đều trả `OperationHandle`)
| Command | Input |
|---|---|
| `checkout` | `{ url, dest: PathGrantId, rev?, depth: Depth, sparse?: SparseSpec, format: 31 \| 32, storePristine, ignoreExternals }` |
| `update` | `{ wcId, paths?, rev?, depth? }` |
| `switch` | `{ wcId, url, rev? }` |
| `commit` | `{ wcId, paths, message, keepLocks, autoAddRemove: true }` |
| `revert` | `{ wcId, paths, removeUnversioned? }` |
| `add_paths` / `delete_paths` / `move_path` | `{ wcId, paths }` / `{ wcId, paths, keepLocal? }` / `{ wcId, from, to }` (tên có hậu tố để tránh từ khóa Rust `move` và tên chung chung) |
| `cleanup` | `{ wcId, removeUnversioned?, removeIgnored?, vacuumPristines? }` |
| `set_depth` | `{ wcId, path, depth }` |
| `upgrade_format` | `{ wcId, to: 32 }` — **sau MVP** |
| `resolve` | `{ wcId, path, choice: 'mine' \| 'theirs' \| 'working' \| 'base' }` |
| `props_set` | `{ wcId, path, name, value \| null }` |

`type Depth = 'empty' | 'files' | 'immediates' | 'infinity'`;
`type SparseSpec = { path: string; depth: Depth \| 'exclude' }[]`.

### 3.4 Remote (không cần WC)
| Command | Input | Output |
|---|---|---|
| `repo_info` | `{ url }` | `{ repoRootUrl, repoUuid, youngestRev, kind }` |
| `repo_list` | `{ url, rev? }` | `RepoEntry[]` |
| `repo_cat` | `{ url, rev? }` | `TextSide` (giới hạn kích thước; quá lớn → lỗi, dùng `repo_save`) |
| `repo_save` | `{ url, rev?, grant: PathGrantId }` (file chọn qua `path_pick` kiểu `saveFile`) | `OperationHandle` (ghi thẳng ra đĩa) |
| `log_get` | `{ target: LogTarget, fromRev?, toRev?, limit, stopOnCopy?, withPaths: true, filter? }` | `{ entries: LogEntry[], nextRev?: Rev }` (cache-first). Phân trang: mới nhất trước; `fromRev` là **cận trên bao gồm** (bỏ trống = HEAD); `entries` có tối đa `limit` phần tử (1–1000); `nextRev` là revision đầu tiên **chưa được trả về**, nên lần gọi kế tiếp truyền `fromRev = nextRev` (bao gồm); không có `nextRev` nghĩa là hết. `filter` khớp không phân biệt hoa thường trên message và author, áp dụng trước khi cắt trang |
| `blame_get` | `{ url, rev? }` | `OperationHandle` → kết quả `{ lines: { rev, author, date, text }[] }` |
| `lock` / `unlock` | `{ targets: LockTargets, comment?, steal? }` / `{ targets: LockTargets, breakLock? }` | `OperationHandle` |
| `remote_edit` | `{ ops: RemoteOp[], message }` (mkdir/rename/delete/copy/upload; `upload` dùng `source: PathGrantId`, không nhận đường dẫn thô) | `OperationHandle` |
| `branches_list` | `{ repoRootUrl }` | `BranchNode[]` (theo layout, cache 60s; rỗng khi `mode: 'none'`) |
| `layout_get` / `layout_set` | `{ repoUuid }` / `{ repoUuid, layout }` | `Layout` / `void` |
| `layout_detect` | `{ repoRootUrl, url }` | `{ found: boolean; layout?: Layout; current?: BranchRef }` |
| `copy_location` | `{ fromUrl, fromRev?, toUrl, message, switchWcId? }` (tạo branch/tag hoặc bản sao bất kỳ) | `OperationHandle` |

### 3.5 Hệ thống
| Command | Mô tả |
|---|---|
| `op_cancel { opId }` | Hủy tác vụ |
| `op_list` | Tác vụ đang chạy + 50 gần nhất |
| `auth_respond { requestId, response }` | Trả lời prompt: `{kind:'credentials',username,password,save}`, `{kind:'negotiate'}` (SSO, chỉ khi `offered` có `negotiate`), `{kind:'certificate',decision}` hoặc `{kind:'cancel'}`. Prompt không trả lời sau 5 phút coi như hủy |
| `accounts_list`, `accounts_sign_out { realm }` | Quản lý credential |
| `settings_get`, `settings_set { patch }` | Settings (§6.3); khóa dạng `"general.checkoutRoot"`, `patch` chỉ chứa khóa cần đổi. `SettingsPatch` **không có** các khóa chạy chương trình (`diff.externalTool`, `diff.mergeTool`, `tools.editor`, `tools.terminal`); khóa lạ bị từ chối khi deserialize |
| `program_choose { kind, args }`, `program_clear { kind }` | Đặt/xóa chương trình ngoài (`kind`: `diff-tool` \| `merge-tool` \| `editor` \| `terminal`). Rust mở dialog chọn file native rồi dialog xác nhận native (mỗi tham số một dòng, có quote; từ chối ký tự điều khiển/bidi); trả `Settings` mới (không đổi nếu người dùng hủy) |
| `path_pick { request }` | Dialog native chọn thư mục / file / nơi lưu (`PickRequest`), trả `PathGrantInfo \| null` (xem §3.6) |
| `reveal_in_os { wcId, path }`, `open_external { wcId, path, with: 'default' \| 'editor' \| 'terminal' }` | Tích hợp OS |
| `diagnostics_export` | Tạo gói chẩn đoán zip, trả đường dẫn |

### 3.6 Đường dẫn cục bộ ngoài WC: PathGrant
WebView **không bao giờ** gửi đường dẫn thô cho thao tác đọc/ghi ngoài WC đã đăng ký (thư mục checkout, nơi lưu `repo_save`, file upload, WC cần thêm). Những đường dẫn đó đến từ dialog native (`path_pick`) hoặc kéo-thả native; Rust giữ đường dẫn thật và trả một mã mờ:
```ts
type PathGrantId = string;                         // mờ, dùng một lần
interface PathGrantInfo { id: PathGrantId; displayName: string; kind: 'file' | 'dir'; }
type PickRequest =
  | { kind: 'folder'; title?: string } | { kind: 'file'; title?: string }
  | { kind: 'saveFile'; suggestedName: string; title?: string };
interface DroppedPaths { grants: PathGrantInfo[]; position: { x: number; y: number }; }  // event fs://dropped
```
Grant dùng một lần, hết hạn sau 5 phút, gắn với cửa sổ `main`; dùng sai loại (file/thư mục) cũng làm grant mất hiệu lực. Đường dẫn trong WC đã đăng ký là relpath, được kiểm bằng `RelPath` (chặn `..`, tuyệt đối, ký tự lạ). Rust xử lý `WindowEvent::DragDrop` và phát `fs://dropped`; UI không được hành động theo đường dẫn từ API kéo-thả của JS (không cấp quyền window nên API đó không dùng được).

## 4. Lỗi
```ts
interface AppError {
  code: ErrorCode; message: string;            // message đã dịch, hiển thị được
  detail?: string;                             // kỹ thuật: mã svn (E160028), HTTP status, URL
  path?: string; retryable: boolean;
  actions?: ('retry' | 'update' | 'cleanup' | 'sign-in' | 'trust-cert' | 'open-lock-dialog' | 'open-settings')[];
}
type ErrorCode = 'network' | 'timeout' | 'auth-required' | 'forbidden' | 'tls-untrusted'
  | 'out-of-date' | 'locked-by-other' | 'lock-token-invalid' | 'wc-busy' | 'wc-needs-cleanup'
  | 'wc-format-unsupported' | 'not-a-working-copy' | 'conflict-blocks-commit' | 'hook-failed' | 'repository-moved' | 'pristine-not-hydrated' | 'wc-invalid-operation' | 'path-not-found'
  | 'disk-full' | 'permission-denied' | 'file-in-use' | 'cancelled' | 'invalid-argument' | 'internal';
// invalid-argument: giá trị sai, grant hết hạn/đã dùng, tùy chọn không hỗ trợ (MVP chỉ ghi format 31)
```

## 5. Events (Rust → UI)
| Event | Payload | Dùng cho |
|---|---|---|
| `op://started` | `OperationHandle` | Activity |
| `op://progress` | `Progress` (throttle ≤ 10/s/op) | Activity, nút Update |
| `op://finished` | `{ opId, result }` (kết quả theo loại, vd `{ newRev }`) | toast, invalidate query |
| `op://failed` | `{ opId, error: AppError }` | Activity, banner |
| `wc://status-changed` | `{ wcId, changed: FileStatus[], removed: string[] }` (delta) | tab Changes |
| `wc://revision-changed` | `{ wcId, baseRev, revRange? }` | toolbar, History |
| `wc://incoming` | `{ wcId, count, latestRev }` | badge Update, thông báo OS |
| `wc://banner` | `{ wcId, kind, data }` (conflicts, mixed-rev, lock-stolen, needs-cleanup) | Banner |
| `auth://prompt` | `{ requestId, kind: 'credentials' \| 'certificate', realm, url, cert?, reason?, attempt, offered }` | Dialog Sign in / Trust |
| `fs://dropped` | `DroppedPaths` (§3.6) | Thả thư mục/file vào cửa sổ |

Payload `auth://prompt`: với `kind: 'credentials'` có `reason: 'initial' | 'rejected' | 'negotiate-failed' | 'expired'` (chọn câu chữ dialog), `attempt` (số lần thất bại, từ 0) và `offered: ('basic' | 'negotiate')[]`; với `kind: 'certificate'` thì `reason` vắng, `attempt` là 0 và `offered` rỗng. `cert.failure` là `unknown-issuer | self-signed | expired | not-yet-valid | hostname-mismatch | revoked | { changed: { oldSha256, newSha256 } } | { other: string }`. Khi nhiều lỗi cùng áp dụng, báo lỗi nặng nhất: hostname-mismatch, expired, revoked, not-yet-valid đứng trên `changed`, `changed` đứng trên `self-signed`/`unknown-issuer`; cert vừa đổi vừa sai hostname phải báo `hostname-mismatch`. `changed` không bao giờ "luôn tin cậy" mà không có thao tác thay pin rõ ràng.

Payload `auth://prompt`: với `kind: 'credentials'` có `reason: 'initial' | 'rejected' | 'negotiate-failed'` (chọn câu chữ dialog), `attempt` (số lần thất bại, từ 0) và `offered: ('basic' | 'negotiate')[]`. Với `kind: 'certificate'`, `cert.failure` là `unknown-issuer | self-signed | expired | not-yet-valid | hostname-mismatch | revoked | { changed: { oldSha256, newSha256 } } | { other: string }`. Khi nhiều lỗi cùng áp dụng, báo lỗi nặng nhất: hostname-mismatch, expired, revoked, not-yet-valid đứng trên `changed`, `changed` đứng trên `self-signed`/`unknown-issuer`; cert vừa đổi vừa sai hostname phải báo `hostname-mismatch`. Quyết định cho `auth_respond` (`kind: 'certificate'`): `reject | acceptOnce | acceptAlways | replacePin`. Với `changed` chỉ hợp lệ `reject` và `replacePin` (thay pin sau khi người dùng xác nhận); `acceptOnce`/`acceptAlways` không hợp lệ cho `changed`, `replacePin` chỉ hợp lệ cho `changed`, `acceptOnce`/`acceptAlways` chỉ cho `unknown-issuer`/`self-signed`; mọi lỗi khác (hostname-mismatch, expired, not-yet-valid, revoked, other) là lỗi cứng, chỉ hợp lệ `reject` (cố ý chặt hơn svn CLI, không có "accept temporarily"). UI không phải ranh giới tin cậy: transport kiểm quyết định với lỗi thực tế và coi tổ hợp sai là `reject`.

Luồng prompt auth: engine gặp 401 → phát `auth://prompt`, **tạm dừng** operation → UI hiện dialog → `auth_respond` → engine tiếp tục hoặc hủy (`cancelled`).

## 6. Mô hình dữ liệu ứng dụng

### 6.1 `app.sqlite`
```sql
CREATE TABLE working_copy (
  id TEXT PRIMARY KEY, name TEXT NOT NULL, root_path TEXT NOT NULL UNIQUE,
  repo_uuid TEXT NOT NULL, repo_root_url TEXT NOT NULL,
  sort_order INTEGER, added_at TEXT NOT NULL, last_opened_at TEXT
);
CREATE TABLE repo_layout (
  repo_uuid TEXT PRIMARY KEY, mode TEXT NOT NULL CHECK (mode IN ('auto','custom','none')),
  trunk TEXT, branches TEXT, tags TEXT                                -- JSON array, chỉ khi mode='custom'
);
CREATE TABLE recent_url (url TEXT PRIMARY KEY, last_used_at TEXT NOT NULL);
CREATE TABLE trusted_cert (               -- khóa theo host + port + sha256: tin cậy một cổng không áp sang dịch vụ khác cùng host
  host TEXT NOT NULL, port INTEGER NOT NULL, sha256 TEXT NOT NULL, subject TEXT, expires_at TEXT, trusted_at TEXT NOT NULL,
  PRIMARY KEY (host, port, sha256)
);
CREATE TABLE account (                    -- metadata, KHÔNG có mật khẩu; khóa theo origin (không chỉ realm)
  scheme TEXT NOT NULL, host TEXT NOT NULL, port INTEGER NOT NULL, realm TEXT NOT NULL,
  username TEXT, method TEXT NOT NULL CHECK (method IN ('basic','negotiate')),
  last_used_at TEXT
,
  PRIMARY KEY (scheme, host, port, realm)
);
CREATE TABLE wc_lock_owner (               -- sổ sở hữu WC_LOCK (wc.db không lưu PID)
  wc_root TEXT NOT NULL, dir TEXT NOT NULL, pid INTEGER NOT NULL, boot_id TEXT NOT NULL,
  start_time INTEGER,                      -- thời điểm khởi động process (giây): phân biệt pid bị tái sử dụng
  acquired_at TEXT NOT NULL,
  PRIMARY KEY (wc_root, dir)
);
CREATE TABLE commit_message_history (id INTEGER PRIMARY KEY, repo_uuid TEXT, message TEXT, used_at TEXT);
CREATE TABLE setting (key TEXT PRIMARY KEY, value TEXT NOT NULL);   -- JSON value
```

### 6.2 `cache.sqlite` (xóa được bất kỳ lúc nào)
```sql
CREATE TABLE log_entry (repo_uuid TEXT, rev INTEGER, author TEXT, date TEXT, message TEXT,
  PRIMARY KEY (repo_uuid, rev));
CREATE TABLE log_path (repo_uuid TEXT, rev INTEGER, path TEXT, action TEXT, kind TEXT,
  copy_from_path TEXT, copy_from_rev INTEGER);
CREATE INDEX log_path_by_path ON log_path (repo_uuid, path, rev DESC);
CREATE TABLE log_coverage (repo_uuid TEXT, path TEXT, from_rev INTEGER, to_rev INTEGER);  -- khoảng đã tải đủ
CREATE TABLE status_snapshot (wc_id TEXT, path TEXT, data BLOB, PRIMARY KEY (wc_id, path));
CREATE TABLE blob_cache (repo_uuid TEXT, path TEXT, rev INTEGER, sha1 TEXT, size INTEGER,
  last_access TEXT, PRIMARY KEY (repo_uuid, path, rev));                                 -- nội dung ở thư mục cache/blobs
```
Log theo path dùng `log_path_by_path`; khi khoảng revision chưa nằm trong `log_coverage` → gọi server rồi ghi bổ sung.

### 6.3 Settings (key chính)
| Key | Kiểu | Mặc định |
|---|---|---|
| `general.checkoutRoot` | string | `~/svn` |
| `general.checkIntervalMin` | number | 10 |
| `general.language` | `'vi' \| 'en'` | theo OS |
| `appearance.theme` | `'system' \| 'light' \| 'dark'` | system |
| `diff.mode` | `'split' \| 'unified'` | split |
| `diff.ignoreWhitespace` | boolean | false |
| `diff.externalTool` | `{ path, args } \| null` (chỉ đổi qua `program_choose`; `args` có thể chứa `{left}`/`{right}`, xem `diff_external`) | null |
| `diff.mergeTool` | `{ path, args } \| null` (chỉ đổi qua `program_choose`) | null |
| `tools.editor` | `{ path, args } \| null` (chỉ đổi qua `program_choose`) | null |
| `tools.terminal` | `{ path, args } \| null` (chỉ đổi qua `program_choose`) | null |
| `network.maxConnections` | number 1–8 | 4 |
| `network.timeoutSec` | number | 60 |
| `network.proxy` | `'system' \| { url }` | system |
| `wc.defaultFormat` | 31 (MVP chỉ ghi 31; 32 sau MVP) | 31 |
| `wc.readSubversionConfig` | boolean | true |
| `cache.maxSizeGb` | number | 2 |
| `integrations.webRevisionUrl` | string | `https://{server}/!/#{repo}/commit/r{rev}/` |
| `integrations.webFileUrl` | string | `https://{server}/!/#{repo}/view/{revSpec}/{path}` — `{revSpec}`: `head` hoặc `r<N>`; `{path}`: relpath từ repo root, percent-encode UTF-8 từng segment, giữ `/` |
