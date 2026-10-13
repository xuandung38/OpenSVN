# Review PR #34: P05 svn-diff (head `fea095f`)

**Verdict: REQUEST CHANGES.** Phần port chạy đúng: tôi đã kiểm byte-exact với svn 1.15.0 vượt xa corpus của PR. Còn 3 việc nhỏ (license NOTICE, .gitattributes cho fixture, contract `is_binary`) cần sửa trước khi merge. Vấn đề hiệu năng/hủy giữa chừng (vấn đề có sẵn từ thuật toán của svn) để lead quyết: sửa luôn hay tách issue follow-up.

## Phạm vi
- `crates/svn-diff/**` (src 2,0k LOC, 549 fixture / 2,4 MB, `tests/gen/gen_golden.py`) + `Cargo.lock` (tự sinh, hợp lệ theo plan.md:84). Không đụng file ngoài path được giao.
- So sánh từng dòng với upstream 1.15.0: `lcs.c`, `diff3.c`, `diff_file.c` (prefix/suffix, token, unified4, merge3 + context saver), `util.c` (normalize_buffer), `io.c`/`validate.c` (binary).

## Gates (tự chạy trên bản sao trong scratch, toolchain stable 1.98.1)
| Gate | Kết quả |
|---|---|
| `cargo fmt --all --check` | OK |
| `cargo clippy --workspace --all-targets --all-features --locked -D warnings` | OK |
| `cargo test --workspace --locked` | OK (svn-diff: 13 unit + 2 golden + 7 proptest) |
| `cargo deny check` | OK (advisories/bans/licenses/sources) |
| CI của PR | xanh trên cả 3 OS |
| unwrap/expect/panic/unsafe trong lib | không có (chỉ có `#[allow(clippy::too_many_arguments)]` ở lcs.rs:210) |

## Kiểm chứng ngoài corpus của PR (tất cả khớp byte-for-byte với svn 1.15.0 local)
- 300 bộ ngẫu nhiên tôi tự sinh, **trộn `\n`/`\r\n`/`\r` lẫn lộn trong cùng file**, có NUL, 0xFF, không có newline cuối, biến thể whitespace. Mỗi bộ chạy unified với 5 bộ option (`none`, `-x -w`, `-x -b`, `--ignore-eol-style`, `-b --ignore-eol-style`) + merge3 postpone: 0 lệch.
- Input bệnh lý 20k dòng (`noeol_cr`, `lowvocab`, `abab`): unified + merge3 khớp svn.
- Style `OnlyConflicts` (so với output `dc` của trình resolve tương tác của svn, chạy qua pty) trên 7 case nhiều conflict, có cả `@@` và context đầu/cuối: khớp.
- Port trung thành: thứ tự snake, refcount/freelist, bỏ qua token unique, sentinel EOF, đoạn fix-up trong `resolve_conflict`, bỏ đoạn lcs ở EOF trong diff3, `@@ -0,0`, `\ No newline at end of file`, marker dính liền dòng không có EOL (`M|||||||`).

## Success Criteria của phase-05
| Tiêu chí | Trạng thái |
|---|---|
| ≥40 unified + ≥40 merge3 golden khớp svn 1.15.0 | **ĐẠT** (53 / 65 × 3 mode) |
| Display diff đúng nội dung 100% (proptest) | **ĐẠT** |
| Proptest merge3(b,x,b)=x, (b,b,y)=y, (b,x,x)=x không conflict | **ĐẠT** |
| Bench: diff 20k < 50ms, merge3 100k < 300ms | **CHỈ ĐẠT VỚI INPUT HIỀN** (bench dùng dòng gần như unique). Input bệnh lý thì vượt xa ngưỡng, xem I-4 và I-5 |
| Style `modified_latest`, `only_conflicts` | Có code; `only_conflicts` tôi đã kiểm khớp svn; `modified_latest` chưa có golden (rủi ro thấp) |

---

## IMPORTANT

### I-1. Thiếu NOTICE / bản LICENSE cho code port từ Apache Subversion
- Bằng chứng: repo không có `LICENSE`/`NOTICE` (`git ls-files` ở root không có). Phần ghi công chỉ nằm ở header module (lib.rs:3-5, lcs.rs:3, diff_core.rs:4, merge.rs:3, unified.rs:3, affix.rs:3, tokens.rs:3). Apache-2.0 §4(a)/(d) yêu cầu kèm bản License và nội dung NOTICE của upstream khi phân phối tác phẩm phái sinh; app sẽ ship dạng binary (P14).
- Cách sửa (trong path P05 sở hữu): thêm `crates/svn-diff/LICENSE-APACHE` (văn bản Apache-2.0) và `crates/svn-diff/NOTICE` gồm dòng "Apache Subversion / Copyright The Apache Software Foundation / This product includes software developed ... (https://www.apache.org/)" + một câu "Portions of this crate are derived from libsvn_diff (lcs.c, diff.c, diff3.c, diff_file.c, util.c, token.c) of Apache Subversion 1.15.0, modified (C pointer rings → index arenas, in-memory buffers)". Mở ticket cho P14 để gộp NOTICE vào bundle/About.

### I-2. Fixture CRLF/mixed sẽ bị âm thầm chuẩn hóa thành LF khi regenerate
- Bằng chứng: `.gitattributes` ở root (từ #23) có `* text=auto eol=lf`. Index hiện có 61 file `i/crlf` + 10 file `i/mixed` (`git ls-files --eol crates/svn-diff/tests/golden`), lọt qua được chỉ vì commit trước khi `.gitattributes` có mặt. Trên một bản clone sạch, `git add --renormalize crates/svn-diff/tests/golden` sửa **71 file** (`crlf-clean/base`, `expected.*`, ...). Lần sau ai chạy `gen_golden.py` rồi `git add` là mất coverage CRLF. Test có thể vẫn xanh vì input và expected bị chuẩn hóa cùng nhau.
- Cách sửa: thêm `crates/svn-diff/tests/golden/.gitattributes` với `* -text` (hoặc `* binary`). File nằm trong path P05 sở hữu, không cần contract PR.

### I-3. Contract `is_binary(bytes, None)` lệch với quyết định diff/merge của svn
- Bằng chứng: binary.rs:19-31 ghi "Reports whether `bytes` should be treated as binary" và chạy heuristic khi không có mime. Tôi kiểm thực nghiệm với svn 1.15: file có NUL, **đã xóa `svn:mime-type`**, thì `svn diff` vẫn ra hunk text và `svn update` vẫn merge text (`G`). Heuristic (`svn_io_is_binary_data`) chỉ dùng khi `svn add`/auto-props để **đặt** mime. Nếu 11U/12b dùng `is_binary(bytes, None)` để bỏ qua merge3 cho file đã versioned thì sẽ sinh conflict mà svn không sinh.
- Kèm theo, mime parsing lệch `svn_mime_type_is_binary` (validate.c:94-102 dùng `strcspn(mime, "; ")`): binary.rs:27-28 dùng `trim()` + so sánh nguyên chuỗi, nên `image/x-xbitmap; charset=x` bị coi là binary (svn: text), còn `" text/plain"` bị coi là text.
- Cách sửa: tách thành `mime_type_is_binary(&str)` (luật quyết định diff/merge, port đúng `strcspn`) và `looks_binary(&[u8])` (chỉ dùng lúc add/detect). Doc ghi rõ: "versioned file without svn:mime-type is always text for diff/merge". Lead nên làm rõ lại câu trong phase-05 (Requirements: "heuristic … cho file chưa có mime").

### I-4. Độ phức tạp O(N·D) và bùng nổ bộ nhớ, không có cách hủy (lỗi gốc từ svn, nhưng app là GUI)
Đo bằng bản release trên máy M-series (tôi chạy svn 1.15 thật song song để so):

| Input (base/mine/theirs) | Op | OpenSVN | svn 1.15 |
|---|---|---|---|
| `noeol_cr` 20k (7 token, CR-only) | unified | 1,1 s / **1,3 GB RSS** | 1,4 s / 1,3 GB |
| `noeol_cr` 100k | unified / merge3 | **44 s / 35 s, 5,4 GB RSS** | (cùng độ phức tạp) |
| `lowvocab` 20k (4 token ngẫu nhiên) | merge3 | 2,1 s | 2,25 s user |
| `lowvocab` 100k | merge3 | **195 s** | ~O(N²) tương tự |
| `reverse` 20k (dòng unique, đảo ngược) | unified | 2,4 s | 5,1 s |

- Port trung thành nên "không chậm hơn svn" (tiêu chí MVP) vẫn đạt. Nhưng `merge3`/`unified` không hủy được, nên một file sinh tự động/CSV ít giá trị khác nhau có thể treo update cả phút hoặc ăn vài GB rồi OOM app.
- Cách sửa (chỉ thêm API, không phá API cũ): `merge3_with_cancel(.., cancel: &dyn Fn() -> bool) -> Result<MergeResult, Cancelled>`, kiểm cancel mỗi vòng `p` trong `lcs()` (lcs.rs:271-288). Có thể thêm budget `max_cost` (p·(M+N)) để 11U fallback "đánh dấu conflict, không auto-merge". 11U phải chạy trong `spawn_blocking`. Ghi vào doc crate.

### I-5. Display diff (imara-diff) chậm bệnh lý với sửa 1 dòng trên file dòng lặp
- Bằng chứng: `abab` (a/b xen kẽ, mod = dịch 1 dòng): `diff()` 20k = **220 ms** (NFR < 50 ms), 100k = **12 s**. Gọi imara-diff 0.2 trực tiếp (Histogram/Myers/MyersMinimal) trên 100k: ~39-40 s cả ba. Cùng input đó, `unified()` (LCS port) chỉ mất 1,7 ms ở 20k và 8 ms ở 100k. Đây là đường UI.
- Cách sửa (lead chọn, vì plan bắt dùng imara cho display): (a) dựng `Hunk` hiển thị từ `diff2` chunk (đã có, nhanh, khớp svn), bỏ imara (YAGNI); hoặc (b) giữ imara nhưng đặt giới hạn cost/timeout rồi fallback sang `diff2`. Thêm case bệnh lý vào `benches/diff_merge.rs` (hiện chỉ có input hiền, benches/diff_merge.rs:11-21).

## MODERATE

### M-1. EOL dự phòng hardcode `\n`, khác `APR_EOL_STR` của svn trên Windows
- merge.rs:288 `detect_eol(mine).unwrap_or(b"\n")`; svn dùng `APR_EOL_STR` (diff_file.c:2327-2331), tức `\r\n` trên Windows. `index_header` (unified.rs:45-47) hardcode `\n`, trong khi `UnifiedOptions.eol` đã cho cấu hình.
- Cách sửa: thêm `MergeOptions.fallback_eol` và tham số eol cho `index_header` (hoặc `UnifiedOptions.index_path`), mặc định theo `cfg!(windows)` nếu muốn khớp svn từng OS.

### M-2. Golden chưa phủ option ignore-space/eol và style `ModifiedLatest`/`ResolvedModifiedLatest`
- Code đúng (300 bộ của tôi × 4 option set khớp svn), nhưng repo không có regression guard. Cách sửa: thêm chế độ `-x -w|-b|--ignore-eol-style` vào `gen_golden.py`, mỗi loại vài case; với `only_conflicts` thì có thể capture `dc` qua pty (`script -q`).

### M-3. Overflow nhóm hunk trong display
- display.rs:100 `2 * opts.context` sẽ tràn khi `context > usize::MAX/2` (panic khi build debug, nhóm sai khi build release). Dùng `opts.context.saturating_mul(2)`. Giá trị context đến từ UI/IPC.

### M-4. API chỉ nhận buffer trong bộ nhớ, chưa có hướng dẫn cho 11U với file lớn
- Ước tính ~100 B/dòng metadata (Pos 24 B, line slice 16 B, LcsNode 40 B, interner) cộng 3 buffer. Với update, `&[u8]`/`Vec<u8>` vẫn ổn (không ép UTF-8, tốt). Chưa cần API streaming. Đề nghị 11U đặt ngưỡng kích thước (vd. > 64 MB hoặc >1M dòng thì không auto-merge, đánh conflict). Ghi chú này vào doc của `merge3`.

### M-5. Kết thúc gói: checkbox của phase-05 chưa cập nhật (quy tắc 9)
- Tôi không sửa file plan. Lead cập nhật sau khi I-1..I-3 xong. Bench thì đánh dấu "đạt với input hiền" kèm link báo cáo này.

## Ghi chú thấp / quan sát (không chặn)
- `ConflictLabels::default()` là `.mine/.base/.theirs`, khác default của libsvn (`<<<<<<< path`). Không sao vì caller luôn truyền `.mine/.rN/.rM`.
- `ConflictRegion` cũng được ghi cho style `Modified`/`Latest` (vẫn có conflict). Nên doc rõ để 11U biết `conflicts.is_empty()` mới là tín hiệu "sạch".
- Cargo.lock thêm hashbrown 0.15 + foldhash 0.1 trùng phiên bản (do imara-diff); deny vẫn cho qua.
- `gen_golden.py`: không có hostname/secret, chỉ dùng `file://` trong tempdir với `--config-dir` riêng. Kích thước fixture 2,4 MB chấp nhận được.

## Việc nên làm theo thứ tự
1. I-2 `.gitattributes` cho golden (5 phút).
2. I-1 NOTICE + LICENSE-APACHE trong crate + ticket P14.
3. I-3 tách/viết lại doc binary API, sửa `strcspn`.
4. Lead quyết I-4/I-5: sửa trong PR này hay tách issue (đề nghị: issue follow-up chặn 11U/12b, kèm bench bệnh lý).
5. M-1..M-3 có thể gộp vào commit follow-up.

## Câu hỏi chưa giải quyết
- Display diff: giữ imara-diff theo plan (thêm giới hạn cost) hay chuyển sang LCS port cho cả UI? Cần lead quyết, vì đổi quyết định trong phase-05.
- App có cần output giống svn **theo từng OS** (EOL `\r\n` trên Windows) hay chuẩn hóa `\n` mọi nơi? Câu trả lời quyết định M-1.
- Ai sở hữu NOTICE gộp ở cấp app/bundle (P14 hay lead)?

Status: DONE
Summary: Port libsvn_diff đúng byte-for-byte với svn 1.15.0, kể cả trên 300 bộ ngẫu nhiên mixed-EOL/NUL/ignore-option, input bệnh lý và style only-conflicts. REQUEST CHANGES vì 3 việc nhỏ (NOTICE Apache, .gitattributes cho fixture, contract is_binary). Hiệu năng O(N·D)/không hủy được và imara chậm bệnh lý thì để lead quyết.

---

## Re-review (head `b1d5329`, 2026-10-13)

**Verdict: APPROVE.** Mọi finding IMPORTANT đã được xử lý và tôi đã kiểm lại từng cái. Còn 2 follow-up không chặn merge (R-1, R-2).

### Gates (chạy local, `CARGO_BUILD_JOBS=2`, toolchain stable 1.98.1)
| Gate | Kết quả |
|---|---|
| `cargo fmt --all --check` | OK |
| `cargo clippy --workspace --all-targets --all-features --locked -D warnings` | OK |
| `cargo test --workspace --locked` | OK. svn-diff: golden 4/4 (thêm `modified_latest`, `resolved_modified_latest`), `cancel_and_limits` 5/5, proptest OK |
| `cargo deny check` | OK |
| unwrap/expect/panic/unsafe trong `src/` | không có |
| Path ngoài `crates/svn-diff/**` do PR tự sửa | chỉ `Cargo.lock` (đã bỏ imara/hashbrown 0.15/foldhash 0.1). Các file khác trong diff đến từ việc merge `main` |

### Đối chiếu từng finding
| Finding | Trạng thái | Bằng chứng |
|---|---|---|
| I-1 NOTICE/ASF | **Đã sửa** | NOTICE ở root (lead, `bba90b3`) liệt kê libsvn_diff + upstream NOTICE. 7/7 file port có ASF header + dòng "Modified by OpenSVN". `control.rs`/`display.rs`/`lib.rs` là code mới, không cần header |
| I-2 `.gitattributes` | **Đã sửa** | `crates/svn-diff/tests/golden/.gitattributes` = `* -text`. `git check-attr text` cho ra `unset`. Index giữ 72 file crlf + 17 file mixed |
| I-3 binary contract | **Đã sửa** | `diff_as_binary_by_mime` port đúng `strcspn("; ")` + `strncmp` trên toàn chuỗi (binary.rs). `None` là text. Doc phân biệt rõ với `detect_binary_for_add`. Unit test phủ `; charset`, khoảng trắng đầu, `x-xbitmapx` |
| I-4 cancel/budget | **Đã sửa, đạt yêu cầu < 100 ms** | Có `tick` ở mỗi snake (lcs.rs) và mỗi vòng diff3. Đo bằng bản release, cancel ở 300 ms, mỗi op 3 lần, load avg ~80: noeol_cr 100k: unified ≤ 0,28 ms, display ≤ 1,9 ms, merge3 ≤ **55 ms** (trường hợp tệ nhất, chủ yếu là thời gian drop arena ~2,5 GB). lowvocab 100k: ≤ 3,2 ms. abab 100k merge3: ≤ 1,6 ms. `max_cost` giới hạn được cả bộ nhớ, ~10 B/đơn vị (noeol_cr 100k: 1e6 → 14 ms/28 MB, 1e7 → 82 ms/118 MB, 1e8 → 0,76 s/1 GB, trả về `TooComplex`) |
| I-5 display | **Đạt cho probe abab** | abab 20k: **1,7 ms** (trước 220 ms), abab 100k: **10 ms** (trước 12 s). Xem R-1 cho các input bệnh lý khác |
| M-1 EOL | **Đã sửa** | `native_eol()`, `MergeOptions.fallback_eol`, `index_header(path, eol)`. Golden ép dùng `\n` (fixture ghi trên Unix) |
| M-2 golden | **Đã sửa** | Thêm 15 case `opt-*` (-w/-b/--ignore-eol-style, có random) sinh bằng svn thật. `ModifiedLatest` được kiểm trên ≥ 20 fixture conflict. `ResolvedModifiedLatest` có 1 case |
| M-3 overflow | **Đã sửa** | `saturating_mul(2)`, `saturating_sub`, `get(..)` trong display.rs |
| M-4 doc file lớn | **Đã sửa** | Doc của `merge3` ghi ~100 B/dòng, yêu cầu chạy trên blocking thread, ngưỡng 64 MB / ~1M dòng, hoặc dùng `max_cost` |

### Kiểm chứng lại với svn 1.15.0
- 300 bộ ngẫu nhiên (mixed EOL, NUL, 0xFF, whitespace, không có EOL cuối; seed giống lần đầu, sinh lại bằng svn thật). Unified × 5 option set + merge3 postpone đều khớp byte. Display hunk dựng lại đúng file mới. Tổng **0 lệch**.

### Follow-up (không chặn merge)
- **R-1 (MODERATE). Display không còn hiền với mọi input bệnh lý.** Từ khi chuyển sang LCS, display có cùng chi phí O(N·D) như `unified`. Ở 20k dòng (load ~80): noeol_cr 1,6-2,1 s, lowvocab 4,6 s, reverse 3,6-9 s. imara trước đó chỉ mất 3 ms với reverse. Mặc định `DisplayOptions.diff.max_cost = None` nên UI có thể quay vài giây cho tới khi bị hủy. Đề nghị: 12b/P15 luôn đặt `max_cost` (vd. `1e7`, tương đương ~100 MB và ~80 ms) và hiện "diff quá phức tạp" khi nhận `TooComplex`. Hoặc P05 export một hằng `DISPLAY_DEFAULT_MAX_COST` và dùng làm mặc định cho `DisplayOptions`. Ghi thêm mức ~10 B bộ nhớ trên mỗi đơn vị cost vào doc của `max_cost`.
- **R-2 (LOW).**
  - (a) Root chưa có file văn bản `LICENSE` Apache-2.0. NOTICE chỉ dẫn URL, mà §4(a) yêu cầu kèm bản License. File này thuộc lead/P14.
  - (b) Hai test thời gian thực (`< 50 ms`, `< 100 ms`, lấy best-of-N) vẫn qua ở debug khi load ~80, nhưng có thể flaky trên CI yếu. Nên giữ ngưỡng rộng hoặc gate bằng `#[ignore]` cho CI nightly.
  - (c) `golden.rs` khi gặp file `opts` không nhận ra thì âm thầm rơi về `DiffOptions::default()`. Nên `panic!` để không bỏ sót.

Status: DONE
Summary: Re-review b1d5329 — APPROVE. I-1..I-5 + M-1..M-4 đã sửa và được kiểm thực nghiệm: cancel ≤ 55 ms, abab display 1,7 ms ở 20k, 300 case svn 0 lệch, gates xanh. Follow-up: đặt mặc định max_cost cho display (R-1), thêm file LICENSE ở root.
