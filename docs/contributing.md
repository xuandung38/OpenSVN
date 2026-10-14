# Contributing

OpenSVN: SVN desktop client. Engine Rust thuần (`crates/`), UI Tauri 2 + React (`apps/opensvn`). Đọc `README.md` và `docs/system-architecture.md` trước khi làm.

## Lệnh
```bash
pnpm install --frozen-lockfile
pnpm build                     # bắt buộc trước cargo (Tauri nhúng dist/)
pnpm exec biome check .        # lint + format web
pnpm typecheck && pnpm test
pnpm gen:ipc                   # sau khi đổi crates/opensvn-ipc; commit bindings.ts
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked --no-tests=pass
cargo test -p svn-wc --features failpoints --locked
cargo xtask compat up|fixtures|run   # harness so với svn thật, cần Docker
```

## Quy tắc
- Không dùng libsvn/svn CLI trong engine; working copy phải tương thích svn chuẩn (wc.db format 31/32).
- Mã/SQL port từ Apache Subversion: giữ header ASF, thêm "Modified by OpenSVN", cập nhật `NOTICE`.
- `specta`/`tauri-specta` pin bằng `=`; không tự nâng phiên bản đã pin trong README.
- Không sửa tay `apps/opensvn/src/ipc/bindings.ts`.
- Fixtures không chứa host thật (`fixtures/check-no-real-hosts.sh`).

## Nhánh git

**Mẫu tên**
- `feat/<ten-tinh-nang>`: thêm hoặc mở rộng hành vi.
- `fix/<ten-loi>`: sửa lỗi của hành vi đã có.
- Viết thường, kebab-case (chỉ `a-z`, `0-9`, `-`), không dấu, ngắn và nói rõ việc. Được thêm mã phase như repo đang dùng: `feat/p08-app-backend-m1`.

| Tên | | Lý do |
|---|---|---|
| `feat/commit-dialog` | Đúng | |
| `fix/commit-recovery-delete-5xx` | Đúng | |
| `feat/p08-app-backend-m1` | Đúng | có mã phase |
| `proteam/abc` | Sai | tiền tố của app, chỉ được ở local |
| `Feat/CommitDialog` | Sai | có chữ hoa |
| `feat/sửa-lỗi` | Sai | có dấu |
| `feature/x` | Sai | tiền tố phải là `feat` |
| `bugfix/x` | Sai | tiền tố phải là `fix` |
| `feat_x` | Sai | thiếu `/`, dùng `_` |

**Chọn feat hay fix**
- `feat` khi thêm hoặc mở rộng hành vi; `fix` khi sửa lỗi của hành vi đã có.
- Có cả hai thì chọn theo mục đích chính.
- Việc khác (docs, ci, refactor) cũng dùng `feat/`, trừ khi sếp quy định thêm.

**Nhánh `proteam/*`**
- App ProTeam tạo `proteam/<topic>` cho worktree của mỗi topic. Nhánh này chỉ được ở local; remote không bao giờ có `proteam/*`.
- Không tự tạo nhánh `proteam/*`. Không đổi tên tại chỗ (`git branch -m`) vì app đang dùng nhánh đó cho worktree.
- Khi đưa lên remote thì push sang tên đúng (sửa lỗi thì dùng `fix/<x>`):
  ```bash
  git push -u origin proteam/<x>:feat/<x>
  ```
  Viết lại `<x>` theo mẫu nếu cần, ví dụ `proteam/e4572f42-rule-dat-ten-nhanh-cho-doi` thành `feat/rule-dat-ten-nhanh-cho-doi`.
- Nếu lỡ đã có `proteam/*` trên remote: push sang tên đúng trước, rồi xoá nhánh cũ:
  ```bash
  git push origin --delete proteam/<x>
  ```

**Quan hệ với app ProTeam**
- Mỗi topic có một worktree và một nhánh `proteam/*` do app tạo.
- App commit theo task khi task done (commit conventional, không nhắc AI).
- Agent không tự `git push`, không đổi nhánh, không merge.
- Thấy nhánh `proteam/*` cần đưa lên thì báo Lead. Lead dựng lệnh push và đưa qua nút chạy của sếp.
- Merge vào `main` qua PR.

## Cấu trúc
- `crates/` svn-types, svn-delta, svn-diff, svn-protocol, svn-ra(-api), svn-wc, svn-client, secure-store, opensvn-ipc
- `apps/opensvn/src` (React), `apps/opensvn/src-tauri` (command Tauri)
- `tests/compat`, `docker/`, `fixtures/`, `xtask/`: compat harness
- `docs/`, `plans/`
