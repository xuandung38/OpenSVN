---
phase: 1
title: "Workspace bootstrap and shared contracts"
status: pending
priority: P1
effort: "01a: 1 tuần · 01b: 1.5 tuần · 2 agent song song"
dependencies: []
wave: 0
owner_paths:
  01a: ["Cargo.toml", "rust-toolchain.toml", "deny.toml", ".cargo/", "crates/svn-types/", "crates/svn-ra-api/", "crates/*/Cargo.toml + src/lib.rs skeleton (svn-ra lib.rs khóa)"]
  01b: ["crates/opensvn-ipc/", "apps/opensvn/ (skeleton)", "package.json", "pnpm-workspace.yaml", "biome.json", ".github/workflows/ci.yml", ".github/CODEOWNERS"]
---

# Phase 1: Workspace bootstrap and shared contracts

## Overview
Hai gói chạy song song để mở khóa wave 1 sớm nhất:
- **01a — contract engine** (merge cuối tuần 1 → mở P02–P07).
- **01b — Tauri/IPC/CI** (merge tuần 2 → mở P08, P09). 01b rebase lên 01a.
Contract phải **compile được** (không phải comment) và đã qua 1 spike chứng minh mô hình editor/async.

## Requirements
- Functional: workspace build 3 OS; contract đủ để các gói wave 1 code bằng fake mà không đổi contract trong 3 tuần đầu.
- Non-functional: `svn-types` **không phụ thuộc tokio/I/O**; CI < 15 phút; mọi version dependency pin chính xác (gồm `specta`/`tauri-specta` — pin bản đã kiểm tra build được).

## Architecture

### 01a — `svn-types` (sync, thuần dữ liệu)
```rust
pub struct Revnum(pub u64);
pub struct RelPath(String);   // '/', NFC, không rỗng đoạn, không '..', không '.'
impl RelPath {
  pub fn new(s: &str) -> Result<Self>;          // nội bộ
  /// Bắt buộc cho MỌI path đến từ server/repo (update-report name, log path, list entry, props externals):
  /// từ chối '..', '.', '/', '\\', ':' (ADS), NUL/control, thành phần `.svn` (không phân biệt hoa thường),
  /// tên cấm Windows (CON, PRN, AUX, NUL, COM1–9, LPT1–9, kể cả có đuôi), khoảng trắng/dấu chấm cuối (Windows).
  pub fn from_server(s: &str) -> Result<Self>;
}
/// Nối an toàn vào wcroot: không theo symlink ở bất kỳ thành phần trung gian (lstat từng cấp), kết quả phải nằm dưới root.
pub fn safe_join(root: &Path, rel: &RelPath) -> Result<PathBuf>;
pub struct RepoUrl(url::Url);
pub struct OriginKey { pub scheme: String, pub host: String, pub port: u16, pub realm: String } // khóa credential
pub enum NodeKind { None, File, Dir, Symlink, Unknown }
pub enum Depth { Empty, Files, Immediates, Infinity, Exclude, Unknown }
pub enum Checksum { Md5([u8;16]), Sha1([u8;20]) }
pub struct PropChange { pub name: String, pub value: Option<bytes::Bytes> }
pub struct LockInfo { pub path: String, pub token: String, pub owner: String, pub comment: Option<String>, pub created: Timestamp, pub expires: Option<Timestamp> }
pub struct DirEntry { /* name, kind, size, created_rev, last_author, time, has_props, lock */ }
pub struct LogEntry { pub rev: Revnum, pub author: Option<String>, pub date: Option<Timestamp>, pub message: Option<String>, pub changed_paths: Vec<ChangedPath>, pub has_children: bool }
pub struct ChangedPath { /* path, action A|M|D|R, kind, copy_from, text_mods, prop_mods */ }

/// Trạng thái node do svn-client (P11) trả — P08/P15 map sang FileStatus IPC (doc 03)
pub struct WcStatus {
  pub path: RelPath, pub kind: NodeKind, pub text: NodeStatus, pub props: PropStatus, pub tree_conflict: bool,
  pub moved_from: Option<RelPath>, pub moved_to: Option<RelPath>, pub changelist: Option<String>,
  pub lock: Option<LockInfo>, pub lock_is_mine: bool, pub needs_lock: bool, pub binary: bool, pub size: Option<u64>,
  pub remote: Option<RemoteStatus>,   // out_of_date, remote_rev, remote_lock
  pub switched: bool, pub depth: Depth, pub base_rev: Option<Revnum>,
}

pub struct CancelToken(/* Arc<AtomicBool> + Notify */);   // check(), cancelled() future-agnostic
pub struct SvnError { pub code: ErrorCode, pub message: String, pub svn_code: Option<u32>, pub child: Option<Box<SvnError>> }
pub enum ErrorCode { Network, Timeout, AuthRequired, Forbidden, TlsUntrusted, OutOfDate, LockedByOther, LockTokenInvalid,
  WcBusy, WcNeedsCleanup, WcFormatUnsupported, NotAWorkingCopy, ConflictBlocksCommit, PathNotFound, UnsafePath,
  DiskFull, PermissionDenied, FileInUse, Cancelled, Corrupt, ServerUnsupported, Internal }
// Bảng ErrorCode → IPC ErrorCode + actions (doc 03 §4) nằm trong opensvn-ipc (01b) kèm snapshot test; Corrupt → wc-needs-cleanup, UnsafePath → internal + log.
```
```rust
/// Delta editor SYNC (tương đương svn_delta_editor_t). Chạy trên thread blocking riêng, nhận lệnh qua kênh có giới hạn (xem svn-ra-api::EditorWorker).
/// Quy tắc thứ tự (driver phải tuân, editor phải chấp nhận):
/// 1. Cây duyệt depth-first; nhiều file có thể đang mở cùng lúc (≤ N) — close_file có thể trễ (tải song song).
/// 2. close_directory(D) chỉ được gọi sau khi mọi file/dir con của D đã close.
/// 3. apply_textdelta ≤ 1 lần mỗi file, trước close_file của file đó.
/// 4. abort_edit có thể đến bất cứ lúc nào (hủy/lỗi) — editor phải để WC ở trạng thái hợp lệ (đánh dấu incomplete).
pub trait DeltaEditor: Send {
  fn set_target_revision(&mut self, rev: Revnum) -> Result<()>;
  fn open_root(&mut self, base_rev: Option<Revnum>) -> Result<DirId>;
  fn delete_entry(&mut self, path: &RelPath, rev: Option<Revnum>, parent: DirId) -> Result<()>;
  fn add_directory(&mut self, path: &RelPath, parent: DirId, copy_from: Option<(&str, Revnum)>) -> Result<DirId>;
  fn open_directory(&mut self, path: &RelPath, parent: DirId, base_rev: Option<Revnum>) -> Result<DirId>;
  fn change_dir_prop(&mut self, dir: DirId, change: PropChange) -> Result<()>;
  fn close_directory(&mut self, dir: DirId) -> Result<()>;
  fn absent_directory(&mut self, path: &RelPath, parent: DirId) -> Result<()>;
  fn add_file(&mut self, path: &RelPath, parent: DirId, copy_from: Option<(&str, Revnum)>) -> Result<FileId>;
  fn open_file(&mut self, path: &RelPath, parent: DirId, base_rev: Option<Revnum>) -> Result<FileId>;
  fn apply_textdelta(&mut self, file: FileId, base_checksum: Option<Checksum>) -> Result<Box<dyn WindowSink>>; // WindowSink: Send
  fn change_file_prop(&mut self, file: FileId, change: PropChange) -> Result<()>;
  fn close_file(&mut self, file: FileId, text_checksum: Option<Checksum>) -> Result<()>;
  fn absent_file(&mut self, path: &RelPath, parent: DirId) -> Result<()>;
  fn close_edit(&mut self) -> Result<()>;
  fn abort_edit(&mut self) -> Result<()>;
}
pub struct DirId(pub u32); pub struct FileId(pub u32);         // handle id → object-safe, không cần associated type
pub struct TxDeltaWindow { /* sview_offset, sview_len, tview_len, ops: Vec<TxDeltaOp>, new_data */ }
pub trait WindowSink: Send { fn window(&mut self, w: Option<&TxDeltaWindow>) -> Result<()>; }
pub trait Reporter: Send { fn set_path(..); fn delete_path(..); fn link_path(..); fn finish(&mut self) -> Result<()>; }

/// Sổ sở hữu WC_LOCK ngoài wc.db (app.sqlite do P08 cài; MemoryLockOwnership cho test/CLI)
pub trait LockOwnershipStore: Send + Sync {
  fn record(&self, wcroot: &Path, dir: &RelPath, pid: u32, boot_id: &str) -> Result<()>;
  fn forget(&self, wcroot: &Path, dir: &RelPath) -> Result<()>;
  fn owned_by_dead_process(&self, wcroot: &Path) -> Result<Vec<RelPath>>;  // pid không còn sống hoặc boot_id khác
}
```

### 01a — `svn-ra-api` (async; tokio, async-trait)
```rust
#[async_trait] pub trait AuthPrompter: Send + Sync {
  /// Gọi tối đa 1 lần đồng thời cho mỗi OriginKey (single-flight do transport đảm bảo). None = người dùng hủy.
  async fn credentials(&self, origin: &OriginKey, attempt: u8, offered: &[AuthMethod]) -> Option<Credentials>;
  async fn confirm_certificate(&self, info: &CertInfo) -> CertDecision;
}
pub struct Credentials { pub username: String, pub password: secrecy::SecretString, pub save: bool, pub method: AuthMethod }
pub enum AuthMethod { Basic, Negotiate }
pub struct CertInfo { pub host: String, pub port: u16, pub subject: String, pub issuer: String, pub not_before: Timestamp,
                      pub not_after: Timestamp, pub sha256: [u8;32], pub failure: CertFailure }
pub enum CertFailure { UnknownIssuer, SelfSigned, Expired, NotYetValid, HostnameMismatch, Revoked, Other(String) }
pub enum CertDecision { Reject, AcceptOnce, AcceptAlways } // AcceptAlways chỉ hợp lệ với UnknownIssuer|SelfSigned (transport kiểm)

#[async_trait] pub trait RaSession: Send + Sync {
  fn repos_root(&self) -> &RepoUrl; fn uuid(&self) -> &str; fn session_url(&self) -> &RepoUrl;
  async fn ensure_authenticated(&self, cancel: &CancelToken) -> Result<()>;   // gọi TRƯỚC khi lấy WC_LOCK
  async fn youngest_rev(&self, c: &CancelToken) -> Result<Revnum>;
  async fn check_path / stat / list / get_file(sink: AsyncWrite) / log(on_entry) / get_locks / lock / unlock / file_revs(handler);
  /// Update/switch/status: reporter dựng report, RA drive editor qua EditorWorker (editor chạy spawn_blocking).
  async fn do_update(&self, req: UpdateRequest, report: Box<dyn FnOnce(&mut dyn Reporter) -> Result<()> + Send>,
                     editor: Box<dyn DeltaEditor>, c: &CancelToken) -> Result<()>;
  /// Commit: client DRIVE editor async (như svn_ra_get_commit_editor). Observer nhận txn name + rev để ghi commit journal.
  async fn commit_editor(&self, msg: &str, rev_props: &[PropChange], lock_tokens: &[(String, String)], keep_locks: bool,
                         observer: Arc<dyn CommitObserver>) -> Result<Box<dyn CommitEditor>>;
  async fn abort_txn(&self, txn_name: &str) -> Result<()>;                   // dọn txn rò (best-effort)
}
#[async_trait] pub trait CommitEditor: Send {
  async fn open_root(&mut self) -> Result<DirId>; async fn add_directory(..); async fn open_directory(..);
  async fn add_file(..) -> Result<FileId>; async fn open_file(..) -> Result<FileId>;
  /// Nội dung lấy từ nguồn ĐỌC LẠI ĐƯỢC (file pristine/working) để gửi lại sau 401/lỗi mạng
  async fn apply_text(&mut self, f: FileId, base: Option<ReplayableSource>, target: ReplayableSource, base_md5: Option<Checksum>, result_md5: Checksum) -> Result<()>;
  async fn change_prop(..); async fn delete_entry(..); async fn copy(..); async fn close_file(..); async fn close_directory(..);
  async fn close_edit(self: Box<Self>) -> Result<CommitInfo>;   // MERGE
  async fn abort_edit(self: Box<Self>) -> Result<()>;            // DELETE txn
}
pub trait CommitObserver: Send + Sync { fn on_txn_created(&self, txn: &str); fn on_merged(&self, info: &CommitInfo); }
pub struct ReplayableSource { pub path: PathBuf, pub translated: bool }  // mở lại được nhiều lần
pub struct EditorWorker;  // spawn_blocking + mpsc bounded(64) chuyển EditorCommand → DeltaEditor; trả lỗi editor về driver
```
- Feature `fake`: `FakeRaSession` (repo in-memory: cây, log, lock, **commit editor** tạo revision mới), `RecordingEditor` (ghi chuỗi lệnh, kiểm 4 quy tắc thứ tự), `NullReporter`, `MemoryLockOwnership`.

### 01b — `opensvn-ipc` + app skeleton + CI
- Kiểu doc 03 §2/§4/§5 thành Rust `serde + specta`; **bảng map `svn_types::ErrorCode` → IPC `ErrorCode` + `actions`** + snapshot test; `WcStatus` → `FileStatus` mapping fn.
- `apps/opensvn`: Tauri 2 + React 19 + Vite + TS strict; mọi command doc 03 §3 dạng stub; `capabilities/default.json` tối thiểu (chỉ cửa sổ `main`, chỉ command của app); CSP mặc định chặt (P08 hoàn thiện).
- `bindings.ts` sinh bằng `pnpm gen:ipc`; CI sinh lại và fail nếu lệch.
- CI 3 OS (ubuntu-24.04, windows-2025, macos-15): fmt, clippy, nextest, cargo-deny (advisories, bans, licenses, sources), `pnpm install --frozen-lockfile`, `pnpm audit --prod`, biome, tsc, vitest; actions pin SHA; cache rust/pnpm; Linux cài `libwebkit2gtk-4.1-dev`.
- `.github/CODEOWNERS`: lead cho lockfile, workflow, `crates/svn-types`, `crates/svn-ra-api`, `crates/opensvn-ipc`, `crates/svn-ra/src/lib.rs`.

## Related Code Files
- Create: như trên + skeleton các crate (`svn-delta`, `svn-protocol`, `svn-ra` [lib.rs: `pub mod transport; pub mod auth; pub mod session; pub mod ops;`], `svn-diff`, `svn-wc`, `svn-client`, `secure-store`).
- Modify: `README.md` (Build & Dev), `.gitignore`.

## Implementation Steps
**01a**
1. Workspace + toolchain + `deny.toml` đầy đủ; khai sẵn `[workspace.dependencies]` theo ADR-003 (tokio, bytes, url, thiserror, anyhow, tracing, serde, async-trait, secrecy, quick-xml, base64, flate2, lz4_flex, md-5, sha1, rusqlite bundled, imara-diff, ignore, globset, notify, reqwest+rustls, rustls-platform-verifier, keyring-core, specta…) — pin version.
2. **Spike 1 ngày**: `EditorWorker` + `RecordingEditor` + `FakeRaSession::do_update` checkout cây 3 cấp với close_file trễ → chứng minh mô hình async/sync compile & chạy.
3. **Research 0.5 ngày**: đọc `svn_sqlite.c` (tag 1.14.5 + 1.15.0) → ghi journal mode, busy timeout, locking mode vào `reports/from-lead-svn-sqlite-settings-note.md`; kết quả là **gate** cho 07 và read path P08.
4. `svn-types`: kiểu + `RelPath::from_server` + `safe_join` (test: traversal, `.svn`, symlink trung gian, CON/NUL, ADS, NFC/NFD, hoa thường) + `LockOwnershipStore`.
5. `svn-ra-api`: trait + fake đầy đủ (gồm commit) + doc comment.
6. Skeleton crate; PR `feat/01a-contracts`.
**01b**
7. `opensvn-ipc` + mapping + snapshot.
8. Tauri skeleton, stub command, capabilities/CSP mặc định, `gen:ipc`.
9. CI + CODEOWNERS; README; PR `feat/01b-app-skeleton-ci`.

## Success Criteria
- [ ] `cargo build --workspace` + `pnpm tauri build --debug` xanh trên 3 OS.
- [ ] `cargo tree -p svn-types` không có tokio/reqwest/hyper.
- [ ] Spike: checkout fake 3 cấp qua `EditorWorker` với close_file trễ; `RecordingEditor` xác nhận 4 quy tắc thứ tự; FakeRaSession commit tạo revision mới.
- [ ] Bộ test `RelPath::from_server`/`safe_join` xanh trên 3 OS (Windows: tên cấm, ADS; macOS: NFD).
- [ ] Ghi chú `svn_sqlite.c` đã có trong `reports/`.
- [ ] `bindings.ts` sinh tự động, CI fail nếu lệch; mapping ErrorCode có snapshot.

## Risk Assessment
- Contract thiếu → contract PR, lead duyệt ≤ 24h. Spike ở bước 2 giảm rủi ro mô hình editor.
- `specta`/`tauri-specta` còn thay đổi API → pin chính xác, ghi version trong README.
- 01b phụ thuộc kiểu từ 01a → 01b làm phần Tauri/CI trước, rebase khi 01a merge.
