# Phase 2 — Remote ops (không cần working copy)

## Mục tiêu
Engine `svn-ra` đủ cho repo browser, lịch sử, blame, lock và commit trực tiếp lên server (kiểu svnmucc). Là nền cho phase 3 (update-report, commit) và cho mobile sau này.

## Yêu cầu
| Thao tác | Request |
|---|---|
| info / list | PROPFIND (props `svn:`, kind, size, created-rev, lock) |
| cat theo revision | GET `!svn/rvr` |
| log (paths, phân trang, stop-on-copy) | REPORT `log-report` |
| blame | REPORT `file-revs-report` + svndiff → tính blame client-side |
| diff giữa 2 revision | REPORT `update-report` (text-deltas) hoặc tải 2 bản + `svn-diff` |
| locks | REPORT `get-locks-report`, `LOCK` / `UNLOCK` (token, steal) |
| lịch sử vị trí (copy/rename) | REPORT `get-locations`, `get-location-segments` |
| commit không WC | POST `me` (create-txn) → PUT svndiff / MKCOL / DELETE / COPY / PROPPATCH → MERGE |
| tạo branch/tag | COPY URL→URL trong 1 txn |

- `svn-diff`: line diff + unified output, nhận diện binary (`svn:mime-type`).
- Cache SQLite: log và list theo `(repo_uuid, path, rev)`.

## Kiểm thử
- Differential: log/list/cat/blame/info khớp svn CLI trên fixture.
- Commit: sau khi engine commit, `svn log -v` + `svn cat` xác nhận đúng; abort txn khi lỗi (DELETE txn).

## Tiêu chí xong
- App desktop (bản sớm): đăng nhập, duyệt repo, xem log/diff/blame, lock/unlock, tạo branch/tag — chạy trên Windows/Linux/macOS.
