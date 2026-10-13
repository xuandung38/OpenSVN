# Red team — Security Adversary — review plan MVP (261013-1410)

Phạm vi: plan.md + phase-01..14; đối chiếu docs/system-design/02 §3.3, 03, 01 §4, research reports. Chỉ grep/đọc, không có source code.
Kết luận chung: §3.3 (doc 02) có 6 cam kết bảo mật; plan chỉ hiện thực hóa 2 phần (lọc log, keyring). CSP, Tauri capabilities, escape nội dung repo, kiểm tra đường dẫn do server cung cấp, chặn Basic qua http: **không có bước triển khai, không có owner, không có test**.

## Finding 1: Khóa credential theo `realm` do server tự khai → server lạ lấy được mật khẩu AD
- **Severity:** Critical
- **Location:** P04 Requirements (secure-store key), P08 AuthBroker; doc 02 §2 bảng lưu trữ; doc 03 §6 bảng `account`
- **Flaw:** Key lưu và tra secret = `<realm>:<user>`; `realm` lấy từ `WWW-Authenticate: Basic realm="…"`, server muốn ghi gì cũng được. Không gắn scheme+host+port như svn (`<https://host:443> realm`). Bảng `account` có PK = `realm` → hai server cùng realm dùng chung một credential.
- **Failure scenario:** Người dùng mở WC có `svn:externals` (hoặc URL dán vào Checkout/Switch) trỏ tới `https://evil.example` có cert hợp lệ. Server này trả 401 với realm giống hệt realm VisualSVN nội bộ (`VisualSVN Server`). AuthBroker tìm thấy credential đã lưu và tự gửi Basic `user:password` AD, người dùng không thấy prompt nào. Kẻ tấn công lấy được mật khẩu domain.
- **Evidence:**
  - `phase-04-transport-auth-and-secure-store.md:25` — "`get/set/delete(service="opensvn", key="<realm>:<user>")`"
  - `docs/system-design/02-architecture-data-flow-and-reliability.md:114` — "Key `opensvn:<realm>:<user>`"
  - `docs/system-design/03-ipc-api-contract-and-data-model.md:206` — "`realm TEXT PRIMARY KEY, username TEXT, method …`"
  - `reports/from-researcher-to-planner-svn-http-protocol-v2-report.md:145` — "credentials cached per realm"
- **Suggested fix:** Định nghĩa `RealmKey = (scheme, host lowercase, port mặc định hóa, realm)` trong `svn-types` (contract P01). Key keyring = `opensvn:<scheme>://<host>:<port>|<realm>|<user>`, PK `account` đổi theo. Chỉ tự gửi credential khi origin khớp chính xác; origin mới phải prompt. Thêm test wiremock: 2 server cùng realm, server thứ hai phải nhận prompt chứ không nhận secret.

## Finding 2: Tauri capabilities, CSP và isolation chỉ có trong doc, plan không giao cho phase nào
- **Severity:** Critical
- **Location:** doc 02 §3.3; P01 (skeleton `src-tauri`), P08 (owner `src-tauri/`), P14 (owner `tauri.conf.json` bundle/updater). Grep "CSP" và "capabilit" (nghĩa Tauri) trên plan đều 0 kết quả.
- **Flaw:** Không phase nào được giao việc viết `capabilities/*.json`, `app.security.csp`, scope cho `asset:` protocol, hay danh sách plugin cho phép. P01 tạo stub cho **tất cả** command và đăng ký cho cửa sổ chính, không có allowlist theo cửa sổ. Không có test nào kiểm CSP hay capability.
- **Failure scenario:** Một XSS bất kỳ (Finding 3) chạy được mọi command IPC: `settings_set diff.externalTool {path:"/bin/sh", args:[…]}` rồi mở diff thì thành RCE; `checkout {url, path:"~/Library/LaunchAgents"}` thì ghi file tùy ý; `diagnostics_export` thì lộ dữ liệu. Nếu scope `asset:` để rộng mặc định kiểu `**`, `fetch("asset://localhost/Users/x/.ssh/id_ed25519")` đọc được file bất kỳ.
- **Evidence:**
  - `docs/system-design/02-architecture-data-flow-and-reliability.md:147` — "WebView: CSP chặt, không tải nội dung remote; Tauri capabilities giới hạn command theo cửa sổ."
  - `phase-01-workspace-bootstrap-and-shared-contracts.md:33` — "src-tauri/ Tauri 2 skeleton + mọi command stub"
  - `phase-08-app-backend-core-services.md:9` — owner `apps/opensvn/src-tauri/` nhưng mục Requirements (dòng 18–30) không có CSP/capability.
  - `docs/system-design/03-ipc-api-contract-and-data-model.md:66` — "`image?: { leftUrl?: string; rightUrl?: string }; // asset:// URL tạm`"
  - `docs/system-design/03-ipc-api-contract-and-data-model.md:236` — "`diff.externalTool` | `{ path, args } | null`"
- **Suggested fix:** Giao cho P08 một mục riêng kèm Success Criteria: CSP `default-src 'self'; script-src 'self'; img-src 'self' asset: blob:; connect-src ipc: http://ipc.localhost; object-src 'none'; frame-src 'none'` (không `unsafe-inline` cho script); bật `withGlobalTauri:false`; capability chỉ liệt kê command đã có trong doc 03; asset scope chỉ gồm thư mục temp do app tạo. Các setting chạy được lệnh (`diff.externalTool`, editor/terminal) chỉ sửa qua dialog native có xác nhận. Test: snapshot `tauri.conf.json` + capabilities, E2E kiểm `eval` và inline script bị chặn.

## Finding 3: Không có quy tắc escape/sanitize cho nội dung repo hiển thị trong WebView
- **Severity:** High
- **Location:** P09 (DiffView, ImageCompare, RevisionRow, FileRow), P12 (History, Browse preview, Properties, Conflict, banner `post-commit-err`, toast lỗi)
- **Flaw:** Doc 02 chỉ cam kết escape "nội dung file trong diff". Plan còn không đưa cam kết đó vào P09/P12. Các nguồn còn bỏ ngỏ: commit message, author, tên file/thư mục, giá trị prop (`svn:externals`, `svn:mime-type`), `post-commit-err` (output hook server), thông điệp `D:error` từ server/IIS, lock comment, preview SVG/HTML trong Browse, "language theo đuôi file". Grep `sanitiz|innerHTML|dangerouslySetInnerHTML` trên plan = 0.
- **Failure scenario:** Đồng nghiệp (hoặc tài khoản bị chiếm) commit với message hoặc tên file `<img src=x onerror=…>` / đường dẫn kiểu markdown-link. Nếu UI render message dạng rich text (linkify issue, highlight tìm kiếm qua `innerHTML`) hoặc preview `.svg` bằng `<object>`/`<iframe>`, script chạy trong WebView, kết hợp Finding 2 thành RCE trên máy mọi người mở History.
- **Evidence:**
  - `docs/system-design/02-architecture-data-flow-and-reliability.md:148` — "Nội dung file hiển thị trong diff được escape (không render HTML/SVG của repo)."
  - `phase-12-frontend-feature-screens-wired-to-ipc.md:19` — "tab **Browse** (cây repo theo revision, bảng, preview, blame…)"
  - `phase-09-frontend-shell-and-design-system.md:49` — "DiffView (CodeMirror 6) + ImageCompare (side-by-side/onion/swipe)."
  - `phase-10-svn-ra-remote-operations.md:23` — "`post-commit-err` → cảnh báo" (chuỗi do server cung cấp, hiển thị lên UI)
- **Suggested fix:** Thêm vào P09 rule Biome cấm `dangerouslySetInnerHTML` (lint `noDangerouslySetInnerHtml` ở mức error) và đưa vào CI. SVG chỉ hiển thị bằng `<img>` (script không chạy) hoặc rasterize ở backend. Tuyệt đối không dùng `<iframe>/<object>` cho preview. Linkify chỉ cho scheme `https:`, mở qua `open_external` đã kiểm. Thêm bộ fixture "hostile repo" ở P06 (tên file/message/prop chứa payload HTML/JS, bidi override U+202E, dấu xuống dòng) và test Vitest khẳng định hiển thị dạng text.

## Finding 4: Đường dẫn do server cung cấp trong update-report không được kiểm tra trước khi ghi đĩa (path traversal, symlink, `.svn`, tên reserved Windows, ADS)
- **Severity:** Critical
- **Location:** P01 contract `RelPath` / `Reporter` / `RaSession`; P03 update parser; P10 `editor_bridge`; P07 M5 translate (special) + M6 workqueue file-install; P11 `WcUpdateEditor`
- **Flaw:** Biện pháp duy nhất là unit test `RelPath` từ chối `..` và chuẩn hóa NFC. Plan không có bước nào cho các trường hợp sau:
  - (a) component `.svn` hoặc `.SVN` (Windows/macOS không phân biệt hoa thường);
  - (b) ghi xuyên symlink: server thêm `svn:special` `link ../../..` rồi thêm file con bên dưới, hoặc symlink trỏ ra ngoài WC rồi file-install ghi theo;
  - (c) tên reserved Windows (`CON`, `NUL`, `COM1`, `aux.txt`), dấu chấm/khoảng trắng cuối, `:` (ADS `file:stream`), `\` trong tên, ký tự điều khiển;
  - (d) va chạm hoa thường hoặc NFC/NFD (`A` vs `a`) ghi đè file khác;
  - (e) đường dẫn tuyệt đối hoặc ổ đĩa (`C:`) trong attr `name`.

  Thêm vào đó, nhiều chữ ký trong contract vẫn nhận `&str` thô (`Reporter::set_path`, `RaSession::get_file/stat/list`, `ChangedPath.path`, `LockInfo.path`), nên kiểu `RelPath` bị bỏ qua.
- **Failure scenario:** Server độc hại, hoặc repo bị kẻ có quyền commit cài bẫy, gửi `S:add-file name=".svn/wc.db"` hoặc `name="x/../../.ssh/authorized_keys"`, hoặc gửi symlink `dir -> /Users/u/Library/LaunchAgents` rồi `add-file dir/evil.plist`. Work queue `file-install` ghi ra ngoài WC, thành persistence/RCE. Trên Windows, `name="file.txt:Zone.Identifier"` ghi ADS, `name="CON"` làm treo hoặc hỏng checkout.
- **Evidence:**
  - `phase-01-workspace-bootstrap-and-shared-contracts.md:43` — "`pub struct RelPath(String); // '/' separator, không '/' đầu/cuối, đã chuẩn hóa NFC; RelPath::new() validate`"
  - `phase-01-workspace-bootstrap-and-shared-contracts.md:137` — "unit test cho `RelPath` (chuẩn hóa, từ chối `..`, NFC)"
  - `phase-01-workspace-bootstrap-and-shared-contracts.md:92` — "`fn set_path(&mut self, path: &str, …`" (không phải RelPath)
  - `phase-07-svn-wc-database-and-pristine-store.md:30` — "special (symlink ↔ "link target") … byte-for-byte như svn" (không giới hạn target)
  - `phase-10-svn-ra-remote-operations.md:22` — "`FetchFile` → GET song song … buffer theo file vào file tạm" (không kiểm path)
- **Suggested fix:** Contract P01 thêm `RelPath::from_server(&str) -> Result<RelPath>` với các điều kiện: từ chối `..`/`.`/rỗng, `/` đầu, `\`, `:`, NUL/control, component `.svn` (không phân biệt hoa thường), reserved Windows, dấu chấm/khoảng trắng cuối (trả lỗi rõ, giống svn trên Windows). Đổi mọi tham số path trong trait thành `RelPath`. P07 thêm hàm `safe_join(wcroot, relpath)`: mở từng component bằng `O_NOFOLLOW` hoặc kiểm `symlink_metadata`, và từ chối khi cha là symlink hoặc reparse point. Symlink target tuyệt đối hoặc thoát khỏi WC được lưu như svn nhưng **không** dùng làm đường ghi. P11 phát hiện va chạm hoa thường/NFC thì báo obstruction. P06 thêm fixture server độc hại (update-report viết tay), P03 thêm fuzz target cho attr `name`, P13 thêm test "không file nào ngoài wcroot bị chạm" (so snapshot FS cha).

## Finding 5: TLS "trust-on-prompt" áp dụng cho mọi lỗi verify, kể cả sai hostname, và pin không có port
- **Severity:** High
- **Location:** P04 TLS; P08 AuthBroker trusted cert; doc 01 §4.4; doc 03 §6 `trusted_cert`
- **Flaw:** Mọi lỗi verify đều dẫn tới prompt và cho chọn `AcceptAlways`, kể cả "sai host", "hết hạn", và (có thể) revoked. Pin lưu `(host, sha256)` không có port. Plan cũng không mô tả cách so pin: so leaf cert hay chain, và khi pin khớp thì có còn kiểm hostname/hạn không. Không có test nào khẳng định cert có fingerprint khác cho cùng host phải prompt lại hoặc bị chặn.
- **Failure scenario:** Trên Wi-Fi khách sạn hoặc proxy TLS doanh nghiệp, kẻ MITM đưa cert tự ký cho `svn.corp.vn`. Người dùng quen bấm "Tin vĩnh viễn" vì VisualSVN nội bộ vốn dùng cert tự ký. Từ đó mọi phiên tới host này đều đi qua MITM, Basic credential (Finding 6) lộ vĩnh viễn. Pin không có port nên cert tin cho `:8443` cũng được dùng cho `:443`.
- **Evidence:**
  - `phase-04-transport-auth-and-secure-store.md:21` — "lỗi verify → gọi `AuthPrompter::confirm_certificate(info)`; người dùng tin → lưu fingerprint SHA-256"
  - `phase-04-transport-auth-and-secure-store.md:45` — "`CertDecision; // Reject | AcceptOnce | AcceptAlways`"
  - `docs/system-design/01-ui-screens-menus-and-interactions.md:153` — "lỗi cụ thể (hết hạn / sai host / CA không tin cậy). Lựa chọn: Từ chối · Tin tạm thời (phiên này) · Tin vĩnh viễn"
  - `docs/system-design/03-ipc-api-contract-and-data-model.md:202-203` — "`host TEXT NOT NULL, sha256 TEXT NOT NULL … PRIMARY KEY (host, sha256)`"
- **Suggested fix:** Chỉ cho `AcceptAlways` khi lỗi là `UnknownIssuer`/self-signed. Sai hostname hoặc revoked thì chặn cứng (chỉ cho `AcceptOnce` kèm cảnh báo đỏ, hoặc không cho gì). Pin = SHA-256 của leaf DER theo `(host, port)`, kèm `expires_at`; khi pin khớp vẫn kiểm hiệu lực thời gian. Fingerprint mới cho host đã pin thì hiện cảnh báo "cert đã đổi" khác với lần đầu. Thêm test wiremock: cert B cho host đã pin A → phải prompt/chặn; cert sai SAN → không có lựa chọn AcceptAlways.

## Finding 6: Plan cho phép Basic qua `http://` và tự hạ Negotiate xuống Basic, trái khuyến nghị của research
- **Severity:** High
- **Location:** P01 `RepoUrl`; P04 Auth; research report §auth
- **Flaw:** Research ghi rõ "never send Basic over plain http", nhưng P04 không có bước nào chặn, và `RepoUrl` chấp nhận `http`. P04 còn "tự fallback Basic khi Negotiate thất bại", không cần người dùng đồng ý và không gắn điều kiện TLS. Kẻ tấn công trên đường truyền có thể xóa `Negotiate` khỏi `WWW-Authenticate` để ép prompt mật khẩu. Không có test cho cả hai trường hợp.
- **Failure scenario:** Người dùng dán `http://svn.corp.vn/svn/x` (VisualSVN có thể bật cả HTTP). Ứng dụng gửi `Authorization: Basic base64(user:ADpass)` dạng cleartext. Trường hợp khác: Windows SSO bị MITM (Finding 5) hạ xuống Basic, người dùng gõ mật khẩu AD vào prompt.
- **Evidence:**
  - `phase-01-workspace-bootstrap-and-shared-contracts.md:44` — "`pub struct RepoUrl(url::Url); // luôn có scheme https/http`"
  - `phase-04-transport-auth-and-secure-store.md:22` — "tự fallback Basic khi Negotiate thất bại"
  - `reports/from-researcher-to-planner-svn-http-protocol-v2-report.md:145` — "never send Basic over plain http."
  - `phase-04-transport-auth-and-secure-store.md:66` — Success Criteria wiremock không có mục http/downgrade.
- **Suggested fix:** P04 từ chối cứng Basic trên `http://` (`ErrorCode::Forbidden` kèm thông điệp "server yêu cầu mật khẩu qua kết nối không mã hóa"). Fallback Negotiate→Basic phải qua prompt có ghi rõ lý do. Redirect policy: tắt auto-redirect của reqwest hoặc chỉ cho redirect cùng origin, và không gắn lại Authorization khi đổi origin. Thêm test: http + 401 Basic → không có header Authorization nào được gửi; redirect sang host khác → không có Authorization.

## Finding 7: URL do server cung cấp (`checked-in href`, `D:href`, `Location`) được GET kèm credential mà không kiểm origin
- **Severity:** High
- **Location:** P03 update parser (`AddFile{checked_in_href}`, `FetchFile{href}`), P10 `fetch_scheduler`, commit `COPY Destination`
- **Flaw:** Skelta lấy nội dung bằng GET tới href server trả về. Plan không yêu cầu href phải là đường dẫn tương đối cùng origin với session, cũng không chặn scheme khác `https`/`http` hay host khác. Transport gắn auth theo state realm (Finding 1), nên dữ liệu và credential đi theo href.
- **Failure scenario:** Server độc hại, hoặc proxy chèn nội dung trên http, trả `D:checked-in/D:href = https://attacker/x` hoặc `http://169.254.169.254/…`. Client GET kèm Authorization đã cache, thành SSRF trong mạng nội bộ, lộ credential, hoặc nội dung giả được cài vào WC. Checksum chỉ kiểm sau khi đã gửi request.
- **Evidence:**
  - `phase-03-svn-protocol-sans-io-dav-layer.md:25` — "`AddFile{checked_in_href}` … `FetchFile{href}` … P10 chuyển sự kiện thành lời gọi `DeltaEditor` + lên lịch GET."
  - `phase-10-svn-ra-remote-operations.md:42` — "`FetchFile{href} ─► scheduler: GET song song (≤ N) ─► file tạm`"
  - `reports/from-researcher-to-planner-svn-http-protocol-v2-report.md:79` — "queue GET on checked-in href (`!svn/rvr/REV/path`)"
- **Suggested fix:** `UrlBuilder` (P03) chuẩn hóa mọi href: chỉ nhận path tuyệt đối dạng `/…` hoặc URL cùng scheme+host+port với `repos_root`, và phải nằm dưới tiền tố stub (`rvr`/`txr`). Sai thì trả `SvnError::Corrupt`. Thêm golden test và fuzz cho href host lạ, `//host`, `javascript:`, `file:`.

## Finding 8: Updater với token repo private gắn trong app, beta tự phát hành mỗi lần merge, khóa updater không có quy trình
- **Severity:** Critical
- **Location:** P14 Requirements và Risk
- **Flaw:** Phương án "GitHub Releases của repo private (token đọc)" nghĩa là token đọc repo (hoặc PAT) được nhúng vào mọi bản cài. Ai giải nén app ra là đọc được toàn bộ source và release, và thường có cả quyền rộng hơn nếu dùng PAT classic. Beta được build **và ký** sau mỗi merge vào `main`. Trong khi đó 9 agent merge song song, phase được tự thêm dependency (plan.md:72). Một PR độc hoặc một dependency bị chiếm sẽ được tự động đẩy dưới dạng update đã ký tới mọi máy beta. Plan không nói khóa minisign lưu ở đâu, ai giữ, rotate thế nào, mất khóa thì làm gì; không có environment protection cho tag `v*`; GitHub Actions không pin SHA.
- **Failure scenario:** Token trích từ binary bị dùng để clone repo, lộ toàn bộ IP. Hoặc dependency npm/crate bị chiếm, merge qua CI xanh, `release.yml` beta ký bằng `TAURI_SIGNING_PRIVATE_KEY`, updater tải về và cài lặng lẽ trên máy dev nội bộ, những máy đang giữ credential AD trong keychain.
- **Evidence:**
  - `phase-14-packaging-signing-and-release.md:22` — "nguồn = GitHub Releases của repo private (token đọc) hoặc server nội bộ; kênh `stable` / `beta`."
  - `phase-14-packaging-signing-and-release.md:23` — "build beta mỗi merge vào `main` (kênh beta)."
  - `phase-14-packaging-signing-and-release.md:26` — "secret ký chỉ trong GitHub Actions secrets/environments có bảo vệ" (chưa có bước cụ thể, chưa có Success Criteria)
  - `plan.md:72` — "Phase khác chỉ được **thêm** dòng vào `[workspace.dependencies]`"
- **Suggested fix:** Bỏ phương án nhúng token. Dùng server phát hành nội bộ hoặc bucket chỉ chứa `latest.json` và artifact (đọc không cần token, hoặc dùng auth của SSO doanh nghiệp). Khóa minisign stable để offline hoặc trong environment `release` có required reviewers. Beta dùng khóa riêng và job ký chỉ chạy sau khi có người phê duyệt. Pin action theo commit SHA. Thêm Success Criteria: "binary không chứa token" (grep artifact), runbook rotate/compromise cho khóa updater.

## Finding 9: Supply chain chưa được kiểm soát: deny.toml chỉ có license, không advisories, không frozen lockfile
- **Severity:** Medium
- **Location:** P01 deny.toml, ci.yml; plan.md quy tắc dependency
- **Flaw:** `deny.toml` được mô tả chỉ gồm allowlist license và cấm OpenSSL. Không bật `[advisories]` (RustSec), `[sources]` (cấm git/registry lạ), cũng không có `pnpm install --frozen-lockfile`, `pnpm audit` hay review dependency mới. Mọi agent được tự thêm dependency mà không qua review riêng. Workflow CI dùng `Swatinem/rust-cache` và action bên thứ ba nhưng không pin SHA.
- **Failure scenario:** Một agent thêm crate typo-squat hoặc crate có advisory RCE. CI vẫn xanh vì không có kiểm advisory, crate chảy vào bản beta đã ký (Finding 8).
- **Evidence:**
  - `phase-01-workspace-bootstrap-and-shared-contracts.md:27` — "deny.toml license allowlist (MIT, Apache-2.0, BSD, ISC, Zlib, Unicode), cấm crate có OpenSSL"
  - `plan.md:95` — "CI xanh mới merge: … `cargo deny check`, `pnpm biome check`, `pnpm test`" (không có audit npm/frozen lockfile)
  - `phase-01-workspace-bootstrap-and-shared-contracts.md:141` — "cache `Swatinem/rust-cache`, pnpm store" (không pin SHA)
- **Suggested fix:** Bật đủ bốn mục `advisories`/`bans`/`sources`/`licenses` của deny.toml, `unknown-registry = "deny"`, `unknown-git = "deny"`. Thêm `pnpm install --frozen-lockfile` + `pnpm audit --audit-level high`. Thêm CODEOWNERS cho `Cargo.toml`, `Cargo.lock`, `pnpm-lock.yaml` (lead phải duyệt dependency mới). Pin action theo SHA và cấu hình Dependabot.

## Finding 10: Test chống lộ log/diagnostics quá hẹp (chỉ `Authorization`/mật khẩu)
- **Severity:** Medium
- **Location:** P04 Success Criteria, P08 Diagnostics, doc 02 §3.3/§3.4
- **Flaw:** Test chỉ bắt `Authorization` và mật khẩu. Các thứ còn bỏ sót:
  - URL có userinfo (doc 02 đòi lọc, nhưng plan không có test);
  - `Proxy-Authorization`; `Cookie`/`Set-Cookie`;
  - lock token `opaquelocktoken:` (ai có token là thao tác được lock);
  - `network.proxy {url}` có `user:pass` lại được xuất vào zip dưới mục "cấu hình (không secret)";
  - commit message, tên file và đường dẫn tuyệt đối có tên người dùng: PII trong log debug ("path rút gọn") đi thẳng vào zip, người dùng thường gửi zip này ra ngoài để báo lỗi;
  - panic hook ghi stack có thể chứa giá trị `Debug` của struct có credential nếu không dùng `SecretString` triệt để.
- **Failure scenario:** Người dùng gửi zip "Report a problem" lên kênh chung. Zip chứa `https://user:pass@proxy:3128`, lock token và log debug có toàn bộ đường dẫn, tên dự án nội bộ.
- **Evidence:**
  - `phase-04-transport-auth-and-secure-store.md:68` — "Không xuất hiện mật khẩu/`Authorization` trong log ở mọi mức (test bắt log)."
  - `phase-08-app-backend-core-services.md:28` — "`diagnostics_export` (zip log + thông tin môi trường, không secret)."
  - `docs/system-design/02-architecture-data-flow-and-reliability.md:145` — "layer lọc header `Authorization`, URL có userinfo."
  - `docs/system-design/03-ipc-api-contract-and-data-model.md:239` — "`network.proxy` | `'system' | { url }`"
- **Suggested fix:** Redaction layer dạng denylist cho header `Authorization`, `Proxy-Authorization`, `Cookie`, `Set-Cookie`, `WWW-Authenticate` (token Negotiate), `Lock-Token`, `If`. Regex cho `opaquelocktoken:`, userinfo trong URL. Proxy URL phải lưu mật khẩu trong keyring, không lưu trong `setting`. Diagnostics hiển thị danh sách file sẽ đưa vào zip và cho xem trước. Test dạng property: sinh request với secret ngẫu nhiên ở mọi header/URL, rồi grep log + zip không thấy secret.

## Unresolved questions
- Có chấp nhận `http://` cho repo nội bộ không, hay chặn hẳn? (cần PO/lead quyết)
- Kênh updater cuối cùng là server nội bộ hay GitHub private? Quyết định này ảnh hưởng trực tiếp Finding 8.
- Có cần tương thích cache credential của svn CLI/TortoiseSVN (`~/.subversion/auth`) không? Nếu có thì format realmstring phải theo `<https://host:port> realm`.

Status: DONE
