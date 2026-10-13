---
phase: 14
title: "Packaging signing and release"
status: pending
priority: P2
effort: "2–3 tuần rải từ wave 2 · 1 agent"
dependencies: [8, 9]
wave: 2-4
owner_paths: [".github/workflows/release.yml", "packaging/", "apps/opensvn/src-tauri/tauri.bundle.conf.json", "apps/opensvn/src-tauri/icons/"]
---

# Phase 14: Packaging signing and release

## Overview
Đóng gói, ký số, phân phối nội bộ và auto-update cho 3 OS. Bắt đầu từ wave 2 để phát hành **M1 Read-only client** và phát hiện sớm vấn đề ký/notarize.

## Requirements
- **Mỗi OS một định dạng**: Windows **MSI** (WiX, ký Authenticode, WebView2 bootstrapper); macOS **DMG** universal (Developer ID + notarize + staple, entitlements tối thiểu); Linux **AppImage + .deb** (khai báo phụ thuộc runtime: WebKitGTK 4.1, libgssapi/krb5 khi bật Negotiate).
- **Auto-update qua GitHub Releases** (quyết định PO), giảm rủi ro:
  - Repo private **riêng** `OpenSVN-releases` chỉ chứa bản cài + `latest.json` (không có source).
  - Token nhúng trong app: **fine-grained PAT chỉ quyền `Contents: read` trên repo `OpenSVN-releases`**, có hạn dùng; quy trình rotate (bản mới mang token mới trước khi token cũ hết hạn).
  - Update ký minisign; khóa riêng chỉ trong GitHub **environment `release` có reviewer bắt buộc**; runbook khi lộ khóa/token (thu hồi, phát hành khóa mới, thông báo).
- **Không kênh beta tự ký mỗi merge**; bản thử nội bộ = build thủ công không auto-update.
- `release.yml`: tag `v*` → build 3 OS → (chờ duyệt environment) ký → đẩy Release lên `OpenSVN-releases` + `latest.json`; release notes từ conventional commits; actions pin SHA.
- Cấu hình bundle/updater ở `tauri.bundle.conf.json` (merge `--config`), không sửa `tauri.conf.json` của P08.
- Icon tạm + bundle id `vn.opensvn.app` (xác nhận PO).
- Hướng dẫn cài & cập nhật nội bộ → PR `docs/deployment-guide.md` cho lead.
- Non-functional: build tái lập (pin toolchain, lockfile `--locked`); không commit chứng chỉ/khóa.

## Architecture
```
.github/workflows/release.yml
packaging/{macos/entitlements.plist, linux/opensvn.desktop, updater/README.md (runbook)}
apps/opensvn/src-tauri/tauri.bundle.conf.json   apps/opensvn/src-tauri/icons/
```

## Related Code Files
- Create/Modify: như trên.

## Implementation Steps
1. Build không ký 3 OS từ skeleton (wave 2).
2. M1: bản cài đọc-only phát hành nội bộ (thủ công).
3. Repo `OpenSVN-releases` + fine-grained token + environment `release` + minisign + `latest.json`; test cập nhật N-1 → N.
4. Ký Windows/macOS khi có chứng chỉ (nếu chưa: phát hành không ký + hướng dẫn SmartScreen/Gatekeeper, ghi rủi ro).
5. Runbook rotate token/khóa; hướng dẫn cài đặt.

## Success Criteria
- [ ] Tag `v0.1.0` → Release trên `OpenSVN-releases` có MSI, DMG, AppImage, deb.
- [ ] Auto-update N-1 → N thành công 3 OS.
- [ ] Kiểm tra binary: token chỉ truy cập được `OpenSVN-releases` (thử clone repo chính bằng token → bị từ chối).
- [ ] Job ký chỉ chạy sau khi reviewer duyệt environment.

## Risk Assessment
- Token trong binary vẫn trích xuất được → phạm vi chỉ bản cài (đã công khai nội bộ); rotate định kỳ.
- Chưa có chứng chỉ ký → không chặn MVP nội bộ; ghi rõ trong release notes.
