# Review 01a: PR #43, #47, #45, #46 (contract, CI svn, license)

Reviewer: code-reviewer · 2026-10-13 · read-only, không push. CI GitHub không chạy (billing) nên gate chạy local (macOS, rustc 1.98.1, `CARGO_BUILD_JOBS=2`) trên worktree tạm `OpenSVN-worktrees/review-01a`, đã xóa sau khi xong.

| PR | Head đã review | Verdict |
|---|---|---|
| #43 contract: credential request + cert changed | `897ea19` | **APPROVE** (2 Medium nên làm ngay sau, không chặn) |
| #47 contract: HookFailed/RepositoryMoved/PristineNotHydrated + RepoDisplayPath | `c8268f9` | **APPROVE** (sửa 1 doc comment sai, Low) |
| #45 ci: svn native trên 3 OS | `ddb9bcb` | **REQUEST CHANGES** |
| #46 docs: Apache-2.0 | `cf29edb` | **APPROVE** |
| #63 contract: ReplacePin + is_valid_for | `64eef1b` | **REQUEST CHANGES** (AcceptOnce cho lỗi cứng) |

Merge-tree với `origin/main`: cả 4 PR không conflict; #43 và #47 sửa chung `fake/session.rs` và `fake_commit_and_reads.rs` nhưng merge-tree sạch.

---

## PR #43: `CredentialRequest`, `PromptReason`, `CertFailure::Changed`

### Gates (local)
- `cargo fmt --check` ok; `clippy -p svn-ra-api -p opensvn-ipc --all-targets --all-features -D warnings` ok; `cargo test` 2 crate: ok.
- Trong workspace trên `main`, chỉ `svn-ra-api` implement/dùng `AuthPrompter` (grep), nên không có crate nào khác bị vỡ khi merge.

### Đã kiểm
- Ergonomics `#[non_exhaustive]`: struct có `CredentialRequest::new(origin, attempt, offered, reason)` công khai, và `tests/auth_contract.rs` (một crate khác) dựng được bằng nó, nên P04/P08 cũng dựng được. Các field là `pub` nên đọc/destructure được với `..`. `PromptReason` và `CertFailure` là `non_exhaustive`, nên P08 phải có nhánh `_`. Như vậy là đúng ý đồ.
- `CertFailure::Changed { old_sha256, new_sha256 }` là variant kiểu struct, dựng được từ crate ngoài (`non_exhaustive` đặt trên enum, không đặt trên variant). Ok.
- Không có serde/specta trên các kiểu này, nên không lộ thẳng ra IPC; P08 phải map sang DTO (xem M1).
- Debug: `CredentialRequest` chỉ chứa `OriginKey` (scheme/host/port/realm), attempt, offered, reason, không có secret. `Credentials.password` vẫn là `SecretString`.
- `FakeRaSession` đã cập nhật (`Initial` khi attempt 0, `Rejected` từ lần sau), và prompter giả trong test assert đúng điều đó.
- Kiểu `attempt` đổi từ u8 sang u32. Vòng lặp fake `0..3u32` vẫn đúng.

### Findings
- **M1 (Medium) Docs/contract chưa cập nhật.** `plans/.../phase-01-workspace-bootstrap-and-shared-contracts.md:110` vẫn ghi chữ ký cũ `credentials(&self, origin, attempt: u8, offered)`, dòng 117 vẫn ghi `CertFailure` cũ. Doc 03 `docs/system-design/03-ipc-api-contract-and-data-model.md:188`: payload `auth://prompt { requestId, kind, realm, url, cert? }` chưa có `reason`, `attempt`, `offered`, cũng chưa có dạng cert `changed {old,new}`. P08/P09 sẽ phải tự đoán DTO. Sửa: trong PR này (hoặc một PR docs đi kèm, merge cùng lúc), cập nhật snippet ở phase-01 và thêm vào doc 03 `reason: 'initial'|'rejected'|'negotiate-failed'|'expired'`, `attempt`, `offered`, `cert.failure` (kèm hai fingerprint khi `changed`).
- **M2 (Medium) Chưa nói thứ tự ưu tiên khi `Changed` trùng với một lỗi verify khác.** `CertInfo` chỉ có một `failure` (`auth.rs:122-124`). Cert mới có thể vừa khác pin vừa `HostnameMismatch`/`Expired`/`Revoked`. Nếu transport báo `Changed` thì UI có thể mời "AcceptOnce" cho một cert sai hostname. Sửa: thêm vào doc của `Changed` (`auth.rs:93-101`) quy tắc: chỉ báo `Changed` khi lỗi riêng của cert mới là `UnknownIssuer`/`SelfSigned`; các lỗi nặng hơn được ưu tiên báo trước. P04 cần một test cho trường hợp này.
- **L1 (Low)** `new_sha256` trùng với `CertInfo.sha256` (`auth.rs:96-101` và `:122`), hai giá trị có thể lệch nhau. Ghi rõ bất biến `new_sha256 == info.sha256`, hoặc chỉ giữ `old_sha256`.
- **L2 (Low)** Transport không phân biệt được `PromptReason::Expired` (`auth.rs:40`) với `Rejected`, vì server chỉ trả 401 cho cả hai. Nên ghi khi nào được phát `Expired` (ví dụ chỉ khi secure-store có metadata hết hạn); nếu không thì bỏ (YAGNI).
- **L3 (Low)** `tests/auth_contract.rs::credential_request_carries_context` chỉ đọc lại field vừa gán, gần như là test rỗng. Không chặn merge.

Hệ quả rebase: #31 (P04, `client`/auth gọi `credentials(&origin, attempt, &offered)` và `CertFailure::Other("the certificate changed…")`) cần chuyển sang `CredentialRequest` + `Changed`. #44 (P08) chưa implement prompter.

**Verdict: APPROVE.** M1 nên merge cùng lúc (docs), M2 có thể làm ngay sau trong PR của P04.

---

## PR #47: `HookFailed`, `RepositoryMoved`, `PristineNotHydrated`, `RepoDisplayPath` chuyển vào svn-types

Đã review 3 commit (`336dd26`, `d28aa65`, `c8268f9`).

### Gates (local, head `c8268f9`)
- fmt ok; clippy `-p svn-types -p svn-ra-api -p opensvn-ipc --all-targets --all-features -D warnings` ok; test 3 crate ok (gồm snapshot bảng mã lỗi).
- `cargo check --workspace --all-targets --all-features` ok (chạy ở `d28aa65`; `c8268f9` chỉ thêm 1 variant, đã kiểm lại bằng gate theo crate).
- `pnpm gen:ipc` (chạy `gen_ipc` vào `src/ipc/bindings.ts`): **không drift**, `git status` sạch ở `336dd26`, `d28aa65` và `c8268f9`.
- Doc 03 §4 (danh sách `ErrorCode`) có đủ `hook-failed`, `repository-moved`, `pristine-not-hydrated`. Snapshot và `bindings.ts` khớp với thứ tự variant.

### RepoDisplayPath (`crates/svn-types/src/display_path.rs`)
- Field private. Chỉ có `parse`, `from_rel`, `as_str`, `Display`, `Serialize` (thủ công, xuất ra string), cùng các derive `Debug/Clone/PartialEq/Eq/Hash`. **Không có** `Deref`, `AsRef<Path>`/`AsRef<str>`, `From`/`Into` sang `RelPath`/`Path`/`String`, `Borrow`. Đã grep, đạt yêu cầu.
- **Không có `Deserialize`**, nên không có đường dựng giá trị nào bỏ qua `parse`. Đây là lựa chọn đúng: DTO IPC (`opensvn-ipc/models.rs`) vẫn dùng `String`, nên không cần specta.
- Validate cấu trúc: phải tuyệt đối; không có segment rỗng/`.`/`..`; không có ký tự control (`char::is_control` gồm C0, DEL, C1, nên chặn cả NUL và `\n`). Vẫn cho phép `aux.c`, `a:b`, `a\b`, `.svn/x`, `COM1` để hiển thị. Muốn ra RelPath phải gọi `RelPath::from_server(...)` tường minh, và test xác nhận cách này chặt hơn.
- `ChangedPath.path`/`copy_from`, `LockInfo.path`, `FileRev.path` đã chuyển sang kiểu mới. Doc comment cũ ("validate trước I/O") được thay bằng ghi chú dùng để hiển thị.

### Mã lỗi
- `HookFailed`: svn `SVN_ERR_REPOS_HOOK_FAILURE = SVN_ERR_REPOS_CATEGORY_START + 1 = 165001`, đã đối chiếu `svn_error_codes.h` 1.14.x. Map là không retry, không action. Hợp lý.
- `PristineNotHydrated`: đúng phạm vi MVP (phase-07 dòng 18 và 28: WC format 32 đọc được, pristine thiếu thì diff/revert báo cần mạng). Nó thay mã tạm `NOT_HYDRATED_SVN_CODE = 900_001` của #32.

### Findings
- **L1 (Low) Doc comment `RepositoryMoved` dẫn sai hằng svn.** `crates/svn-types/src/error.rs:38` ghi `SVN_ERR_RA_SESSION_URL_MISMATCH` (170011, "Session URL does not match"). ra_serf thật trả `SVN_ERR_RA_DAV_RELOCATED` (175011) cho 301/302/307/308 trên **mọi** request, không riêng request ghi (`libsvn_ra_serf/util.c:1850-1855`, "Repository moved permanently/temporarily to '%s'"). Sửa: đổi tên hằng, bỏ chữ "a write". Câu "validated as an https URL on the same scheme" là cam kết về hành vi của P04/P10, nên chuyển sang doc của producer hoặc thêm test bên đó.
- **L2 (Low)** `RepoDisplayPath::from_rel` (`display_path.rs:41`) không chạy lại kiểm tra control-char. `RelPath::new` (dữ liệu nội bộ) chỉ chặn NUL, nên `from_rel(RelPath::new("a\nb"))` tạo ra một giá trị mà `parse` sẽ từ chối. Thêm `debug_assert!` hoặc ghi rõ trong doc rằng invariant chỉ được đảm bảo khi đi qua `parse`.
- **L3 (Low)** `parse` không chuẩn hóa NFC, còn `from_rel` thì có (vì RelPath đã NFC). Hai giá trị cùng một tên NFD (repo tạo trên macOS) có thể `!=`. Ghi trong doc: không so sánh `RepoDisplayPath` với nhau để suy ra node; muốn so thì đi qua `RelPath::from_server`.
- **L4 (Low, handoff P09)** Ký tự định dạng bidi (U+202E…) không phải `is_control`, nên lọt qua. Không nên chặn ở kiểu dữ liệu (repo có thể chứa chúng thật). FE nên render path log/lock với `unicode-bidi: isolate`/`plaintext` để chống giả mạo hiển thị.
- **Info** `Serialize` hiện chưa có consumer (DTO dùng `String`), nên kéo `serde` vào svn-types chưa cần thiết. Chấp nhận được vì serde không có runtime, nhưng có thể bỏ (YAGNI).
- **Info, rebase** `svn_types::ErrorCode` không phải `non_exhaustive` (có chủ đích, để map IPC phải exhaustive), nên mọi `match` exhaustive ở các PR đang mở sẽ phải thêm 3 nhánh. #40 (P03) phải xóa `RepoDisplayPath`/`DisplayChangedPath`/`RepoLock` local và bỏ `TODO(ErrorCode::HookFailed)`. #44 (P08) có 4 chỗ dựng `LockInfo`/`ChangedPath` cần `.as_str().to_owned()`. #32 (07B) đổi 900_001 sang `PristineNotHydrated`.

**Verdict: APPROVE** (L1 sửa nhanh trong PR hoặc follow-up).

---

## PR #45: svn native trên runner `rust` (Linux build từ source, macOS brew, Windows SlikSVN 1.14.5)

### Đã kiểm (supply chain)
- **sha512 1.15.0**: pin `6bb79686…282ec2` (`ci.yml:33`) trùng với file `.sha512` công bố trên archive.apache.org và downloads.apache.org (đã curl, file chỉ chứa hash nên `awk '{print $1}'` đúng). Pin cũng trùng với `docker/compose.yml:11` của #42.
- Script kiểm cả pin == published và pin == actual, chạy `set -euo pipefail` với `curl --fail`. Lưu ý: so với published lấy từ cùng host không tăng thêm an toàn (ai kiểm soát host thì kiểm soát cả hai file); pin nằm trong repo mới là chốt chặn thật. Không verify GPG, chấp nhận được khi đã có pin.
- **SlikSVN**: tải `https://sliksvn.com/pub/Slik-Subversion-1.14.5-x64.msi` (domain chính thức của SlikSVN): HTTP 200, 7 950 336 byte, sha256 `21aa09ec…756200`, **trùng pin**. Metadata MSI: "Slik Subversion 1.14.5 (x64)", WiX, Author "SlikSvn & The SharpSvn Project". MSI có `svnadmin.exe`, và thư mục cài theo Manufacturer `SlikSvn` khớp với `C:\Program Files\SlikSvn\bin`. Hash được kiểm cả khi cache hit. Tốt.
- **Actions pin SHA**: `actions/cache@55cc8345…` đúng là commit của tag `v6.1.0` (gh api). Các action khác không đổi.
- `permissions: contents: read` ở top-level, không thêm quyền. Cache key gồm OS/arch/version/sha512; cache chỉ được lưu khi job thành công.
- macOS: bước "Check the Subversion tools" (`ci.yml:93-101`) fail khi svn < 1.15.0 (dùng `sort -V`). Windows bỏ qua kiểm version.

### Findings
- **H1 (High) Ngữ nghĩa `OPENSVN_REQUIRE_SVN` mâu thuẫn với helper của P06 (#42).** Mô tả PR #45 ghi: biến này "only turns 'svn missing' into a failure, never 'svn too old for this test'", và test cần 1.15 sẽ skip trên Windows. Nhưng `tests/compat/src/oracle.rs` (#42, `Oracle::resolve_with`) khi `OPENSVN_REQUIRE_SVN=1` lại trả `Err(OracleError::TooOld)` nếu version < min. Doc của helper cũng gọi `MinVersion::V1_15` là "the default expectation". Với env đặt cho cả job (`ci.yml:31`) và Windows chạy 1.14.5, mọi test gọi `Oracle::resolve(MinVersion::V1_15)` sẽ **fail trên Windows** thay vì skip. Phải chốt một quy tắc. Đề xuất, đơn giản nhất và khớp với PR này: sửa ở #42 để `TooOld` luôn skip (có thông báo), còn `REQUIRE` chỉ áp dụng cho `Missing`; version tối thiểu trên Linux/macOS đã do bước CI ở `ci.yml:100` chặn. Lead quyết bên nào sửa; PR còn lại cập nhật mô tả/doc cho khớp.
- **M1 (Medium) Build Linux từ source chưa chạy ở đâu cả.** CI không chạy được, và script khác flag với Dockerfile đã chạy được của #42 (`docker/svn-client/Dockerfile`): thiếu `pkg-config`/`libssl-dev` trong apt; dùng `--with-serf=/usr --with-apr=/usr --with-apr-util=/usr --disable-mod-activation` (`build-svn-from-source.sh:39-43`). Khả năng cao vẫn build được (runner ubuntu có sẵn pkg-config, gói Tauri kéo theo nhiều lib), nhưng đây chỉ là suy đoán. Sửa: thêm `pkg-config` vào apt (`ci.yml` bước "Install Subversion build and runtime libraries"), và đưa bằng chứng một lần chạy, ví dụ `docker run --rm -v "$PWD":/w ubuntu:24.04 bash -c 'apt-get update && apt-get install -y curl ca-certificates build-essential bzip2 libapr1-dev libaprutil1-dev libserf-dev libsqlite3-dev libexpat1-dev zlib1g-dev && /w/.github/scripts/build-svn-from-source.sh 1.15.0 <sha512> /opt/svn && /opt/svn/bin/svnadmin --version --quiet'`. Kèm log vào PR.
- **L1 (Low) Cache key không gồm script build** (`ci.yml:61`). Đổi flag configure thì cache cũ vẫn được dùng nếu không ai nhớ bump `-v1`. Thêm `${{ hashFiles('.github/scripts/build-svn-from-source.sh') }}`.
- **L2 (Low)** Windows không assert version (bỏ qua ở `ci.yml:98`). Nên assert đúng `1.14.5` để nếu sau này đổi MSI thì skip semantics không âm thầm sai.
- **L3 (Low)** `brew install subversion` (`ci.yml:70`) không pin, đi kèm auto-update. Chấp nhận được (bottle có checksum của Homebrew, version có cận dưới), nhưng nên đặt `HOMEBREW_NO_AUTO_UPDATE=1` cho nhanh và ổn định hơn.
- **L4 (Low)** archive.apache.org thường bị rate-limit/chậm; cache giảm được, nhưng lần chạy đầu tiên là điểm dễ fail. Có thể thử downloads.apache.org trước (chỉ chứa bản mới nhất), fallback về archive.

**Verdict: REQUEST CHANGES** (H1 cần lead chốt + một bên sửa; M1 cần bằng chứng build Linux).

---

## PR #46: Apache-2.0

### Đã kiểm
- `LICENSE` **giống từng byte** với `https://www.apache.org/licenses/LICENSE-2.0.txt` (sha256 `cfc7749b…bc523d30` cả hai; 202 dòng; ASCII). Không sửa appendix, đúng chuẩn.
- `[workspace.package] license = "Apache-2.0"` (`Cargo.toml:11`) đã có sẵn; cả 11 crate + `src-tauri` đều dùng `license.workspace = true`. `apps/opensvn/package.json` thêm `"license": "Apache-2.0"`.
- `NOTICE`: header "OpenSVN, Copyright 2026 OpenSVN contributors" + dòng license; phần ghi nhận Apache Subversion giữ nguyên. README dòng 51 đã cập nhật. Merge-tree với main sạch (NOTICE đã có trên main từ #39).

### Findings
- **L1 (Low)** Root `package.json` (`opensvn-workspace`, private) chưa có `license`. Thêm vào cho nhất quán (không bắt buộc vì private).
- **L2 (Low, P12)** `tauri.conf.json` chưa có `bundle.copyright`/`bundle.license`/`licenseFile`, nên installer (MSI/NSIS/DMG) chưa hiện license. Để P12 đóng gói làm.
- **Info** "OpenSVN contributors" không phải pháp nhân. Nếu PO muốn ghi đơn vị sở hữu cụ thể thì đó là quyết định của PO, không phải lỗi.

**Verdict: APPROVE.**

---

## Việc cần làm (ưu tiên)
1. Lead chốt ngữ nghĩa `OPENSVN_REQUIRE_SVN` khi version quá cũ (#45 H1 ↔ #42 `oracle.rs`); một bên sửa.
2. #45: thêm `pkg-config` và đưa log build Linux (docker ubuntu:24.04); thêm hash script vào cache key.
3. #43: cập nhật phase-01 snippet + doc 03 `auth://prompt` (M1); ghi quy tắc ưu tiên `Changed` (M2).
4. #47: sửa doc `RepositoryMoved` (hằng `SVN_ERR_RA_DAV_RELOCATED`, mọi request).
5. Rebase sau khi merge: #31 (AuthPrompter/Changed), #40 (bỏ RepoDisplayPath local, map HookFailed/RepositoryMoved), #44, #32 (PristineNotHydrated).

## Câu hỏi chưa giải quyết
- `OPENSVN_REQUIRE_SVN` + svn quá cũ: skip hay fail? (cần lead quyết; đề xuất skip.)
- `PromptReason::Expired` có nguồn phát thật không, hay bỏ?
- Tên chủ sở hữu bản quyền trong NOTICE/installer (PO).

(Status cuối báo cáo nằm sau mục #63.)

---

## PR #63: `CertDecision::ReplacePin` + `is_valid_for` (head `64eef1b`)

### Gates (local, `CARGO_BUILD_JOBS=2`)
- fmt ok; clippy `-p svn-ra-api -p opensvn-ipc --all-targets --all-features -D warnings` ok; test 2 crate ok.
- `gen_ipc`: không drift (`bindings.ts` có `"replacePin"`).

### Đối chiếu bảng hợp lệ (yêu cầu của lead và code thật)

| failure | Reject | AcceptOnce | AcceptAlways | ReplacePin |
|---|---|---|---|---|
| UnknownIssuer, SelfSigned | yes / yes | yes / yes | yes / yes | no / no |
| Changed | yes / yes | no / no | no / no | yes / yes |
| HostnameMismatch, Expired, NotYetValid, Revoked | yes / yes | **no / YES** | no / no | no / no |
| Other(..), variant tương lai | yes / yes | (no) / **YES** | no / no | no / no |

(Mỗi ô: yêu cầu / PR. Riêng `Other` không được nêu trong yêu cầu; transport của P04 coi nó là lỗi cứng, nên ở đây ghi "no".)

### Findings
- **H1 (High) `AcceptOnce` được coi là hợp lệ cho lỗi cứng.** `crates/svn-ra-api/src/auth.rs:168` `(Self::AcceptOnce, _) => true` trả `true` cho `HostnameMismatch`, `Expired`, `NotYetValid`, `Revoked` và `Other`. Bảng doc ở `auth.rs:138` ("every other failure | yes | yes") và test ở `tests/auth_contract.rs:66-75` **khẳng định** hành vi sai này. Doc 03 dòng 190 cũng ghi `acceptOnce` chỉ bị cấm với `changed`. Hàm này là chốt chặn trust-boundary mà transport phải gọi, nên một webview bị chiếm quyền gửi `acceptOnce` cho một cert sai hostname (MITM) sẽ được contract coi là hợp lệ. Điều này cũng lệch với transport #31 (`tls.rs` module doc: "HostnameMismatch, Revoked, Expired, NotYetValid and anything unrecognized are hard failures: they are never offered to the user"). Sửa theo kiểu allow-list, mặc định đóng:
  ```rust
  match (self, failure) {
      (Self::Reject, _) => true,
      (Self::ReplacePin, CertFailure::Changed { .. }) => true,
      (Self::AcceptOnce | Self::AcceptAlways, CertFailure::UnknownIssuer | CertFailure::SelfSigned) => true,
      _ => false,
  }
  ```
  Cập nhật bảng doc (`auth.rs:131-139`), test (`AcceptOnce` phải là `false` cho 4 lỗi cứng và `Other`) và doc 03 dòng 190 ("`acceptOnce` chỉ cho `unknown-issuer`/`self-signed`").
  Trade-off cần lead/PO biết: svn CLI luôn cho "accept (t)emporarily" với mọi lỗi (`libsvn_subr/prompt.c:718-725`), nên OpenSVN sẽ chặt hơn svn. Nếu PO muốn giống svn thì phải là quyết định có ghi lại, không phải mặc định.
- **M1 (Medium) Nhánh `_` mặc định mở.** Cũng tại `auth.rs:168`: vì `CertFailure` là `non_exhaustive`, mọi variant thêm sau này sẽ tự động được `AcceptOnce`. Bản allow-list ở trên xử lý luôn vấn đề này.
- **L1 (Low, rebase #31)** `CertDecision` giờ là `non_exhaustive`, nên `match decision` của P04 (`client.rs` ~3295) phải có nhánh `_ => Reject`. P04 hiện map `AcceptAlways` + `changed` thành `PinAction::Replace`, cái này phải chuyển sang `ReplacePin`, và phải gọi `is_valid_for` trước khi `tls.accept`.
- **L2 (Low)** `FakeRaSession::ensure_authenticated` await `confirm_certificate` mà không `select!` với cancel (khác #31). Với fake thì chấp nhận được; có thể thêm `cancel.check()?` sau await.
- **Đạt:** `ReplacePin` chỉ hợp lệ cho `Changed`; `Changed` chỉ nhận `Reject`/`ReplacePin`; `AcceptAlways` chỉ cho `UnknownIssuer`/`SelfSigned`; `Reject` luôn hợp lệ. FakeRaSession kiểm bằng `is_valid_for` và trả `TlsUntrusted` (test có 4 decision × Changed, cộng ReplacePin/AcceptAlways cho SelfSigned). IPC `replacePin` dùng camelCase, nhất quán với `acceptOnce`/`acceptAlways` (`#[serde(rename_all = "camelCase")]`, `commands.rs:186`; test serde đủ 4 giá trị). Doc 03 đã cập nhật (nhưng sai ở điểm H1).

**Verdict: REQUEST CHANGES** (H1, M1).

Status: DONE_WITH_CONCERNS
Summary: #43, #47, #46 APPROVE. #45 REQUEST CHANGES (`OPENSVN_REQUIRE_SVN` mâu thuẫn với oracle P06; build Linux chưa được chạy). #63 REQUEST CHANGES: `is_valid_for` cho `AcceptOnce` với hostname mismatch/expired/revoked/not-yet-valid/other và variant tương lai; cần đổi sang allow-list chỉ UnknownIssuer/SelfSigned.
Concerns/Blockers: CI GitHub không chạy; chưa kiểm được clippy cfg riêng Windows và build Linux của #45.
