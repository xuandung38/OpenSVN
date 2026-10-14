# Cài bản thử nghiệm OpenSVN (chưa ký số)

Bản thử nghiệm dùng **engine thật** (cargo feature `release`), kết nối thẳng tới server SVN qua
HTTP/HTTPS. Các bản cài **chưa ký số** và **không tự cập nhật**: mỗi lần có bản mới, anh tải và
cài đè lên bản cũ.

Bản Linux đầu tiên (`v0.0.1-test1`) nằm ở mục Releases của repo (draft, prerelease). Kèm theo bản cài
có `SHA256SUMS` để kiểm tra file tải về.

## Linux (AppImage hoặc .deb)

Yêu cầu: Ubuntu 22.04+/Debian 12+ (hoặc distro có WebKitGTK 4.1), x86_64.

Kiểm tra checksum trước khi cài:

```sh
sha256sum -c SHA256SUMS --ignore-missing
```

**Cách 1: .deb** (Ubuntu/Debian, khuyến nghị)

```sh
sudo apt install ./OpenSVN_0.0.1-test1_amd64.deb
opensvn            # hoặc mở "OpenSVN" từ menu ứng dụng
```

`apt` tự kéo phụ thuộc (`libwebkit2gtk-4.1-0`, `libgtk-3-0`, `libgssapi-krb5-2`: bản Linux liên kết
sẵn thư viện Kerberos). Gỡ cài đặt: `sudo apt remove open-svn` (tên gói xem bằng `dpkg -I <file>.deb`).

**Cách 2: AppImage** (không cần quyền root)

```sh
chmod +x OpenSVN_0.0.1-test1_amd64.AppImage
./OpenSVN_0.0.1-test1_amd64.AppImage
```

Nếu báo thiếu FUSE (`libfuse.so.2`): `sudo apt install libfuse2` (Ubuntu 24.04: `libfuse2t64`),
hoặc chạy không cần FUSE: `./OpenSVN_*.AppImage --appimage-extract-and-run`.

### Kiểm tra nhanh

* Log nằm ở `~/.local/share/com.opensvn.desktop/logs/opensvn.log`. Dòng
  `engine selected engine=Real` xác nhận app đang chạy engine thật.
* Muốn xem giao diện với dữ liệu giả (không đụng server thật): `OPENSVN_ENGINE=fake opensvn`.
  Engine giả lưu dữ liệu riêng trong thư mục `fake-engine`, không lẫn với dữ liệu thật.
* Kerberos/Negotiate: bản Linux bật sẵn GSSAPI. Cần `kinit` trước khi checkout từ server dùng SSO;
  có thể tắt Negotiate trong phần cài đặt mạng nếu server không dùng.
* Gặp lỗi: gửi kèm file log (app tự che mật khẩu, token, header nhạy cảm trước khi ghi).

## Windows và macOS (build từ workflow `release`)

Workflow `.github/workflows/release.yml` build cả ba OS trên GitHub Actions. Hiện runner của repo
lỗi ngay khi khởi động, nên Windows/macOS chưa có bản cài. Khi Actions chạy lại:

1. **Actions → release → Run workflow** (nhánh `main`), điền:
   * `version`: ví dụ `0.0.2` hoặc `0.0.1-2`. MSI (WiX) chỉ nhận pre-release dạng số, nên
     `0.0.1-test2` làm hỏng job Windows.
   * `release_tag`: ví dụ `v0.0.2`. Bỏ trống thì chỉ có artifact, không tạo release.
   * `updater`: để `false`.
2. Hoặc đẩy một tag `v*`: workflow build xong tự tạo **draft release** (không bao giờ tự publish).
3. Tải bản cài ở trang draft release, hoặc ở mục *Artifacts* của lần chạy
   (`opensvn-windows`, `opensvn-macos-universal`, `opensvn-linux`).

Kết quả:

| OS | File | Ghi chú cài đặt |
|---|---|---|
| Windows 10/11 x64 | `OpenSVN_<ver>_x64_en-US.msi`, `OpenSVN_<ver>_x64-setup.exe` | Chưa ký: SmartScreen báo "Windows protected your PC" → **More info → Run anyway**. Bộ cài tự tải WebView2 nếu máy chưa có. |
| macOS 12+ (Intel + Apple Silicon) | `OpenSVN_<ver>_universal.dmg` | Ký ad-hoc, chưa notarize: kéo vào Applications, lần đầu **chuột phải → Open**, hoặc `xattr -dr com.apple.quarantine /Applications/OpenSVN.app`. |
| Linux x64 | `.AppImage`, `.deb` | Như mục Linux ở trên. |

Build tay trên máy từng OS vẫn dùng được: `scripts/release/build-linux.sh`,
`ARCH=universal scripts/release/build-macos.sh`, `scripts\release\build-windows.ps1` (chi tiết trong
`docs/deployment-guide.md`). Các script mặc định bật feature `release` (engine thật; macOS/Linux
thêm `negotiate-gssapi`). Đặt `OPENSVN_FEATURES=""` để build bản chỉ có engine giả.

### Ký số và auto-update (chưa bật)

Workflow để sẵn chỗ, chỉ bật khi có secret/variable tương ứng:

* macOS: `APPLE_SIGNING_IDENTITY`, `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, và để
  notarize: `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`.
* Windows (Authenticode): `WINDOWS_SIGN_COMMAND` (lệnh signtool/Azure Trusted Signing, `%1` là
  file; không đặt mật khẩu trong lệnh).
* Updater: input `updater=true` + secret `TAURI_SIGNING_PRIVATE_KEY` (+ `_PASSWORD`) + variable
  `OPENSVN_UPDATER_PUBKEY`. Phát hành lên repo `OpenSVN-releases` và `latest.json` vẫn làm tay theo
  `packaging/updater/README.md`.
