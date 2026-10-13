# PR #42 review — P06 compat harness M1

Reviewed head `4b1be18906adbc8d17085989fa413fb4b833ca3a` (2 commits). 32 files, +2539/-2.

**Verdict: REQUEST CHANGES** (the compat CI job is red, and the wc.db comparator is flaky and also hides real differences)

## Verified facts (evidence)
- Tarball SHA-512: pins in `docker/compose.yml:7,10` match `archive.apache.org` and `downloads.apache.org` `.sha512` for 1.14.5 and 1.15.0 (fetched 2026-10-13). OK.
- Fixture determinism: ran `make-basic.sh` and `make-nolayout.sh` in local `opensvn-compat/svn-server:1.14.5` and `:1.15.0` images. Both produce `basic=5a57059c…`, `nolayout=23625eb3…`, the same as `fixtures/repos/expected/*`. FSFS format 8 on both. The claim holds.
- Rust gates (scratch copy): `cargo fmt --check` clean, `clippy -p opensvn-compat -p xtask --all-targets -D warnings` clean, 17 unit tests pass (1 ignored), `cargo deny check` = advisories/bans/licenses/sources ok. `serde-saphyr =1.3.0` is safe Rust, deny-clean, and a reasonable choice over serde_yaml forks.
- `gh pr checks 42`: **compat = FAIL** (45 s). rust/tauri ubuntu+windows were still pending at review time. Other checks pass.
- Empirical: in svn 1.15.0, the `NODES.properties` skel blob for a node with 8 props gave **3 different byte orders across 4 checkouts** (APR hash seed per process). wc.db has 16 indexes, 3 triggers, 2 views, and `user_version=31`.

## CRITICAL

### C1. compat CI never builds anything: `.github/workflows/compat.yml:46-53`
`docker/bake-action` with no `source:` uses the remote **git context** (`https://github.com/...git#<merge-sha>`). Compose `context: ./svn-client` is then resolved against the repo root, not `docker/`. The log shows `"/svn-client": not found`, `"/svn-server": not found`. So no step after the build ran: fixture hash check, self-check scenarios and `--ignored` tests. The PR's main claims are unproven in CI.
Fix: `with: { source: ., files: docker/compose.yml, load: true, ... }` (bake then resolves context paths relative to the compose file dir). Alternative: drop bake and use `docker compose -f docker/compose.yml build` with buildx GHA cache via `--set`. Re-run and attach a green run before merge. Because the plan's DoD requires the compat job for P03/07/10/11, a red job blocks every downstream package.

### C2. wc.db comparator is non-deterministic for nodes with ≥2 properties: `tests/compat/src/wcdb_dump.rs:81-96`
Blobs are hex-dumped verbatim. svn serialises the proplist skel in APR-hash order, which changes per process (verified above). The svn-vs-svn self-check passes only because every node in `basic/trunk` has exactly one prop. Any 07A/07B scenario with 2+ props (e.g. `svn:eol-style`+`svn:keywords`, or `svn:mime-type`+`svn:needs-lock`) will flake. Engine-vs-svn comparisons will fail at random even when OpenSVN is correct.
Fix: in `render_cell`, for `NODES.properties`, `ACTUAL_NODE.properties`, `NODES.dav_cache` (and `inherited_props`, which is a list of (path, proplist)), parse the skel and render it canonically with props sorted by name and values hex-encoded. Reusing the svn-wc skel codec from PR #29 would be ideal, but don't depend on an unmerged crate: write a ~40-line skel parser locally or add it later. Add a unit test with two blobs that hold the same props in different order. Also add `svn:eol-style`+`svn:keywords` on one node in `make-basic.sh` (update the hash) so the self-check covers this case.

## IMPORTANT

### I1. Over-normalisation hides real differences: `wcdb_dump.rs:15-23`, `compare.rs:128-135`
- The `time`/`date` substring filter drops `NODES.changed_date` and `LOCK.lock_date`. Both come from the **repository** (identical for any two WCs of the same repo/rev, and fixture dates are pinned), so they are not volatile. If OpenSVN stored a wrong `changed_date`, neither `wcdb` nor `info`/`status` would catch it, because XML `<date>` is dropped too.
- The only genuinely local volatile values are `NODES.last_mod_time`, plus info's `text-updated`/`prop-updated`, which come from it.
- Dropping the whole column also hides NULL vs non-NULL. That matters: a NULL `last_mod_time`/`translated_size` changes svn's status fast path.
Fix: use an explicit allowlist `{NODES.last_mod_time}` rendered as `SET`/`NULL` instead of dropping it. Keep `changed_date`, `lock_date`, XML `<date>`/`<created>`/`<expires>`/`<token>`, which are server-sourced and identical across WCs. If a specific scenario really needs a mask, make it opt-in per scenario (`ignore: [...]`).

### I2. `WORK_QUEUE` is skipped instead of asserted empty: `wcdb_dump.rs:15`
A non-empty work queue after an operation means the op is incomplete, i.e. a real engine bug: the WC needs cleanup, which violates DoD line 129 ("không cần cleanup"). Fix: emit `## WORK_QUEUE rows=<n>`, or fail when `n>0`. `WC_LOCK` may stay skipped, but the same argument applies after a completed op (svn leaves 0 rows), so rendering a count is better.

### I3. Schema is not compared: `wcdb_dump.rs:35`
Only `type='table'` rows are dumped. Indexes (16), triggers (3), views (2) and `PRAGMA user_version` are ignored. A wc.db from OpenSVN with no triggers or a wrong format number would pass `wcdb`, yet svn depends on those triggers (`nodes_insert_trigger` etc.) for ACTUAL/pristine refcount consistency. Fix: dump `user_version`, and `sqlite_master` `type,name,tbl_name,sql` (whitespace-normalised) for index/trigger/view rows.

### I4. Tree comparator ignores the write bit (svn:needs-lock): `compare.rs:73-77`
Only `mode & 0o111` is captured. `svn:needs-lock` files must be read-only in the WC, and `basic/trunk/locked.txt` exists exactly for this. An engine that leaves them writable passes `tree`. Fix: record `readonly: mode & 0o222 == 0` (on Windows, `permissions().readonly()`), and add it to `describe_difference`. Optionally record the whole `mode & 0o777`, but umask differs between container and host, so the bool is safer.

### I5. No negative self-check against real svn
`detects_tree_mismatch` uses a fake backend and covers only `tree`. Nothing proves that `status`/`info`/`wcdb` flag a real difference end to end. Fix: add `scenarios/harness/selfcheck-must-fail.yaml` with an `expect: mismatch` field (new schema field), e.g. b runs `svn propset foo bar README.txt` or `svn update -r1`. The runner must invert the result, and the scenario must assert that each of tree/status/info/wcdb produced ≥1 mismatch. Without this, an over-normalising comparator (I1) always reads green.

### I6. Base images not pinned by digest; no GPG verification: `docker/svn-*/Dockerfile:6,22` (`FROM debian:bookworm-slim`)
The tag floats, so rebuilds aren't reproducible and the cache key drifts. Fix: `FROM debian:bookworm-slim@sha256:<digest>` (both stages, both Dockerfiles), plus a Dependabot docker entry. The SHA-512 pin from archive.apache.org is a TOFU pin and is acceptable for a test harness. The README doesn't claim GPG, so this is not a blocker. Optional: verify the `.asc` against `https://downloads.apache.org/subversion/KEYS` once when updating the pin and record that in the README. apt packages are unpinned (Debian stable point releases): acceptable for test images once the base digest is pinned.

### I7. Root-owned files under `target/compat` on Linux: `docker/compose.yml:32-39` (certs), `:49,61` (logs)
- The `certs` service runs as root: `ca.key`/`server.key`/`*.crt` end up root-owned in the host `target/compat/certs`.
- httpd opens logs as root before dropping to www-data, so `logs/svn1x/*.log` are root-owned too.
- `down --purge` still works, because the parent dirs were created host-owned by `xtask up`. But if anyone runs `docker compose up` directly, Docker creates the dirs as root, and then `cargo clean`/`--purge` fails with EACCES on Linux.
Fix:
- `certs`: add `user: "${HOST_UID:-1000}:${HOST_GID:-1000}"` (gen-certs only needs to write into `/certs`).
- Servers: write logs to `/var/log/svn-httpd` inside a named volume and copy them out in `compat logs`, or run `chown -R "$HOST_UID:$HOST_GID" /var/log/svn-httpd` on exit. Simplest: in the entrypoint, `trap` on TERM to chown the logs to `${HOST_UID}`, passed via env.
- Document that `xtask` is the supported entry point.
- `HOST_UID` defaults to 1000 when `id` fails. Fine.

## MODERATE

- **M1** `tests/compat/scenarios/harness/selfcheck-svn14-svn15.yaml:1-3`: the header says "wc.db is compared too: both write format 31", but `checks: [tree, status, info]` has no `wcdb`. Either add `wcdb` (after C2 is fixed) or fix the comment. Cross-version wc.db equality is exactly what 07A needs as a baseline.
- **M2** `runner.rs:255-263`: status/info for both sides are read with the **server-version** client (`oracle = scn.server`). With `server: svn14`, a WC created by svn15 is read by svn14. That works for format 31 but fails for format 32. Make the reader explicit (`reader: svn15` per compare, default svn15) so future 1.15 format-32 scenarios (DoD l.129) don't silently go through svn14.
- **M3** `compat.yml:19-23`: `push: main` paths omit `xtask/**` and `crates/svn-*`, while `pull_request` includes them. A merge touching only `crates/svn-wc` won't re-run compat on main. Align the two lists.
- **M4** `compat.yml:49-53` cache poisoning: the GHA cache scope `compat-compat` is shared between fork PRs and `main`. GitHub isolates caches written from PR refs (PR scope can't write the base-branch cache), so the risk is low. Add `,ignore-error=true` to `cache-to`, and consider writing the cache only on `push: main` (`cache-to` conditional) to keep PR runs read-only.
- **M5** `compose.yml:42-43,54-55`: `../fixtures` is mounted **rw** into servers that run fixture scripts as root. `--update-hash` legitimately writes `expected/`, but nothing else should. Mount `:ro` and add a separate `:rw` mount for `expected/` only, or have `--update-hash` print the hash and let the dev write it.
- **M6** `runner.rs:154`: a fresh repo name uses `process::id()`. Two concurrent `compat-run` invocations use the same host `work/<name>` dir (`remove_dir_all` at :147), and they also share the hardcoded compose project/ports with any developer's env. Document "one run at a time", or derive the work dir from the same unique suffix.
- **M7** `make-large.sh:22`: `system("mkdir -p " dir)` passes through the shell with a path that contains `$src`. That path comes from `mktemp`, so it's safe in practice. `xtask` validates N as u32. OK, noted only.
- **M8** `compat.yml:56-57`: `rustup show active-toolchain || rustup toolchain install` relies on the runner's preinstalled rustup and has no cache. The other workflow uses an SHA-pinned toolchain action and Swatinem cache, so reuse that pattern for consistency.

## Safety / trust boundary: OK
- Ports `127.0.0.1:8443/8444` only. No `--privileged`, no docker.sock mount, no host network.
- `permissions: contents: read`, no `pull_request_target`, actions SHA-pinned (checkout, setup-buildx, bake), no secrets used.
- Test creds (`tester`/`tester-pass`) and world-readable test keys are clearly labelled test-only and live under `target/`.
- The scenario `name`/`repo`/`wc` validation (`[A-Za-z0-9_-]{1,64}`) closes path-traversal and shell-quote injection in `server_sh` (`runner.rs:163,304`). `sh:` steps run arbitrary shell, but only in client containers, from in-repo YAML (trusted).
- Edits outside owner paths (root `Cargo.toml` members, `.cargo/config.toml` alias, `Cargo.lock`) are minimal and necessary: acceptable. The lead should note them for merge-order conflicts, since other PRs also touch `Cargo.lock`.

## Integration with other workers (07A/07B/P03/P05, dev-01a)
Current state: PR #29/#33/#35 call `Command::new("svn")` directly, gated by `OPENSVN_REQUIRE_SVN` / `OPENSVN_TEST_DOCKER_SVN`. dev-01a is adding native svn ≥1.15 to ci.yml with `OPENSVN_REQUIRE_SVN=1`.

Recommended division (minimal overlap):
1. **Unit-level oracle (fast, per-crate, in ci.yml)**: native svn from dev-01a. Contract:
   - `OPENSVN_SVN_BIN` / `OPENSVN_SVNADMIN_BIN`: path, default `svn`/`svnadmin` on PATH.
   - `OPENSVN_REQUIRE_SVN=1`: a missing or too-old binary is a **test failure**, not a silent skip. Required in CI, which matters on Windows/macOS too.
   - Scope: file:// repos, single version (whatever the runner installs, ≥1.15). Used by 07A/07B (wc.db/skel/pristine round-trips), P05 (`svn diff`/diff3 output parity), P02 (svndiff via `svnadmin dump --deltas`).
   - Put the version check plus lookup in **one** shared test helper, preferably a dev-only `opensvn-test-support` helper owned by 01a (or P06 here), so each crate stops duplicating `Command::new("svn")` + skip logic.
2. **Version matrix + HTTP/DAV oracle (slow, Docker, compat.yml)**: this PR. Used by P03 (DAV protocol captures against real mod_dav_svn), 10a/10b (RA), 11, 07 cross-version WC (1.14.5 vs 1.15.0, TortoiseSVN-oracle 1.14). Engine tests hook in through `EngineDriver` + `scenarios/<owner>/`, not by shelling out to docker themselves.
3. **Drop `OPENSVN_TEST_DOCKER_SVN`** (PR #29's ad-hoc docker run of svn 1.14.5). Cross-version checks belong in this harness as YAML scenarios. Alternatively, bridge with `OPENSVN_SVN_BIN="docker compose -p opensvn-compat exec -T svn14-client svn"`. That is fragile because of path mapping (host paths ≠ `/work`) and file:// repos living on the host, so don't make it the contract. Prefer scenarios.
4. Ask P06 to export a small library API (`opensvn_compat::wcdb_dump::dump_sqlite`, `compare::snapshot_tree`) as a dev-dependency, so 07A unit tests reuse the **same** normaliser (after C2/I1-I3 are fixed) instead of re-implementing it.

## Plan follow-ups (no plan edits made)
- M1 items delivered: images, compose, fixtures (basic/nolayout/large), xtask, DSL, comparators, README with owner dirs. Pending: green compat CI (C1), README environment-verification table (lead TODO), protocol capture set (M2 per README).
- DoD l.129 (format 32 readable) is not yet represented by any scenario. Expected in a later milestone; note M2 above.

## Unresolved questions
1. Is an `expect: mismatch` schema extension acceptable for P06 scope (I5), or should the negative check live as a Rust `#[ignore]` docker test?
2. Should P06 or 01a own the shared `OPENSVN_SVN_BIN` test helper crate?
3. Local images were pre-built by dev-p06; I verified fixtures with those images, not a fresh build of this exact SHA (the Dockerfiles look unchanged in commit 2, but that is not proven).
4. Review was cut short by a host disk-full (ENOSPC) condition. Not re-run: Windows/ubuntu rust CI results (pending at review time).

---

# Re-review — head `baab0e5` (2026-10-13)

**Verdict: APPROVE** (conditional on one green compat CI run once GitHub billing is unblocked; local verification below covers what CI would run).

Diff `4b1be18..baab0e5`: 1 fix commit + merge of main (the ci.yml failpoints step, `fail` dep and svn-wc feature come from main, not P06).

## Local verification (isolated copy: project `opensvn-compat-rv`, host ports 18443/18444, so dev-p06's running env at the same SHA was untouched)
- **C1 FIXED.** `source: ./docker` + `files: ./compose.yml`. bake-action v7.4.0 passes a local `source` as `cwd` (`src/main.ts:116,136,148`), which equals `cd docker && docker buildx bake -f ./compose.yml`. Ran exactly that: `--print` resolves contexts `svn-client` / `svn-server`; `--load` exit 0; all 4 compile stages ran `sha512sum -c`; 5 images tagged.
- **E2E**: `xtask compat up --no-build` ok. `fixtures` gives `basic=314b86a3…` / `nolayout=23625eb3…` on **both** 1.14.5 and 1.15.0. `compat run` passes 4/4 (incl. `selfcheck-must-fail`). `cargo test -- --ignored` 1 passed. Repeated `compat run` ×5: 4/4 PASS every time (no flake with the 8-prop node). `down --purge` removed containers, volumes and network.
- **C2 FIXED.** `skel.rs` + `SKEL_COLUMNS` (NODES.properties/dav_cache/inherited_props, ACTUAL_NODE.properties). Real checkouts a/b have **different raw `properties` blobs** for `many-props.txt` (md5 `bf17…` vs `1747…`), yet their normalised dumps are identical. svn14-vs-svn15 wc.db dump diff = 0.
- **Mutation probes on real wc.db** (scratch test against `dump_sqlite`): each of `changed_date` +1, drop `nodes_insert_trigger`, `user_version=32`, `last_mod_time` → NULL, WORK_QUEUE row, prop value change, extra LOCK row and `translated_size` → NULL **is detected**. A changed `last_mod_time` value (still set) is masked, as designed.
- **Rust**: fmt clean; clippy `-p opensvn-compat -p xtask --all-targets -D warnings` clean; 29 unit + 1 doc test pass; `cargo deny check` all ok.

## Item status
| Item | Status | Evidence |
|---|---|---|
| C1 bake context | Fixed | local bake from docker/ ok |
| C2 skel order | Fixed | real blobs differ, dumps equal; 5× stable |
| I1 over-normalisation | Fixed | only `NODES.last_mod_time` → SET/NULL; XML drops only `text-updated`/`prop-updated`; changed_date/lock kept (probe) |
| I2 WORK_QUEUE | Fixed | `rows=n` in dump + explicit `work_queue_rows` failure in runner |
| I3 schema | Fixed | user_version + index/trigger/view SQL (probe) |
| I4 read-only bit | Fixed | `mode & 0o222 == 0`; host shows `locked.txt`/`many-props.txt` `-r--r--r--` |
| I5 negative control | Fixed (see N1) | `expect: mismatch` + `selfcheck-must-fail.yaml` passes |
| I6 digest | Fixed | `bookworm-slim@sha256:7c7b2c96…` is an OCI index incl. linux/amd64 |
| I7 root-owned files | Fixed (Linux unverified) | certs run as `HOST_UID`; entrypoint touches+chowns logs. macOS bind mounts ignore chown, so the Linux behaviour is reasoned, not observed |
| M1 svn14/15 wcdb | Fixed | `wcdb` added, passes |
| M2 reader | Fixed | `Compare.reader` default svn15 |
| M3 push paths | Fixed | lists aligned |
| M4 cache | Fixed | `cache-to` only on push + `ignore-error=true`, scope `compat-images` |
| M5 fixtures ro | Fixed | `../fixtures:ro` + rw `expected/` overlay; fixtures ran ok |
| M6 concurrency | Documented | README "one scenario run at a time" |
| M8 rust cache | Fixed | Swatinem/rust-cache SHA-pinned |

## Remaining (non-blocking)
- **N1 [MODERATE]** `selfcheck-must-fail.yaml`: `update -r 2` is a coarse difference that every check would flag even with heavy over-normalisation. It proves "not totally blind", not "not lax". The real-wc.db mutation probes above cover that gap. Suggest adding per-check targeted controls later: e.g. b `chmod u+w locked.txt` (tree only), b `svn propset` without commit (status), and a wc.db-only mutation via `sh: sqlite3 .svn/wc.db "update NODES set changed_date=..."` (wcdb only; the client image ships sqlite3).
- **N2 [MODERATE] oracle API** (`tests/compat/src/oracle.rs`), to fix before 07A/07B/P05 adopt it:
  - (a) `svnadmin` is never version-probed, and defaults to PATH even when `OPENSVN_SVN_BIN` points elsewhere, so it can mix versions. Probe it too, or default it to the sibling of `OPENSVN_SVN_BIN`.
  - (b) `Oracle::svn()` inherits the developer's `~/.subversion/config` (`use-commit-times`, `global-ignores`, auto-props, `store-passwords`), so oracle output can differ per machine. Prepend `--config-dir <empty tempdir>` and `--non-interactive`, and set `LC_ALL` to a UTF-8 locale (needed for the Vietnamese paths; macOS has no `C.UTF-8`, use `en_US.UTF-8`).
  - (c) Only the exact value `"1"` enables `OPENSVN_REQUIRE_SVN`, so `true` silently skips. Either accept `1|true` or document "exactly 1".
  - (d) Every test call spawns `svn --version`. Cache it in a `OnceLock`.
  - (e) Dependency direction: svn-wc etc. taking `opensvn-compat` as a dev-dep is fine *only while* opensvn-compat never depends on engine crates. Once an `EngineDriver` impl lands here, split `oracle` + normalisers into a leaf crate (e.g. `crates/opensvn-test-oracle`) to avoid a dev-dep cycle and duplicated types.
- **N3 [LOW]** `skel::render` writes `k=v`, unescaped, and uses hex for atoms with spaces. Collisions like key `a=b`/value `c` vs `a`/`b=c` are theoretical for svn props. `inherited_props` (an ordered list of path/props pairs) gets sorted by path, which loses order information, though root→leaf order usually equals lexicographic order. Acceptable.
- **N4 [LOW]** For `pull_request` events, the `set:` block contains an empty line (the conditional evaluates to `''`). bake-action's input list most likely drops empty items, but this is not verified in CI. Confirm on the first green run.
- Housekeeping: my bake overwrote the local `opensvn-compat/*` image tags with images built from the same Dockerfiles (same SHA), so dev-p06's env is unaffected. Scratch output removed. Worktree `<local>/OpenSVN-worktrees/review-pr42` and local refs `review/pr42`, `review/pr42b` were left in place.

Status: DONE
Summary: All C/I/M items fixed and verified locally (bake from docker/, E2E 4/4 ×5, fixture hashes on both versions, real-wc.db mutation probes); APPROVE pending one green compat CI run. The oracle API needs small hardening (N2) before 07A/07B/P05 adopt it.

---

# PR #50 review — `tests/svn-oracle` (N2 follow-up), head `8c75e19`, base `feat/p06-compat-m1`

**Verdict: APPROVE.**

Local run (`CARGO_BUILD_JOBS=2`, crate-local only): fmt clean; clippy `-p opensvn-svn-oracle -p opensvn-compat --all-targets -D warnings` clean; `OPENSVN_REQUIRE_SVN=1 cargo test -p opensvn-svn-oracle` passed 6 tests plus 1 doc test, and the real-svn test ran against Homebrew svn 1.15.0. A scratch probe also passed:
- `svn add` of `Tài liệu.txt` succeeded.
- The isolated config dir was populated by svn (`config`, `servers`, `auth`, `README.txt`).
- A command built before the `Oracle` was dropped still ran fine; svn recreates the dir.

| N2 point | Status | Where |
|---|---|---|
| (a) svnadmin resolution + version | Done. Sibling of `OPENSVN_SVN_BIN` (`.exe` on Windows), else PATH; explicit override wins; both binaries probed | `lib.rs:99,102-119,182-195` |
| (b) isolation | Done. `svn()` = `--config-dir <TempDir> --non-interactive`; `LC_ALL` is `en_US.UTF-8` on macOS and `C.UTF-8` elsewhere, for svn and svnadmin | `lib.rs:138-169` |
| (c) REQUIRE parsing | Done. `1/true/yes/on` and `0/false/no/off` (case-insensitive); unset = off; anything else, **including empty**, is `Err` | `lib.rs:171-179` |
| (d) caching | Done. Per-process `OnceLock<Mutex<HashMap>>`, errors cached too | `lib.rs:220-233` |
| (e) no cycle | Done. Normal deps are only `tempfile` and `thiserror` (`cargo tree`); `opensvn_compat::oracle` removed | `Cargo.toml` |
| Lead rule | Compliant. REQUIRE fails only on missing or unrunnable binaries; a too-old svn or svnadmin always skips with a `found`/`needs` message (test `:322-342`) | `lib.rs:105-117` |

Non-blocking notes for adopters (07A/07B/P05):
- **[MODERATE] No `file://` URL helper.** The crate's own test builds `format!("file://{}", path.display())`, which is wrong on Windows: it needs `file:///C:/...` with forward slashes. Windows is exactly where SlikSVN 1.14.5 runs, and the tests module is `cfg(unix)`, so this gap is invisible. Add `pub fn file_url(path: &Path) -> String`, plus ideally a `run(&mut Command) -> Result<Output>` that keeps stderr on failure, before three crates each re-implement them.
- **[LOW]** CI runs tests under nextest, which uses one process per test, so the version cache only helps `cargo test`. That costs about one `--version` spawn per test, which is acceptable.
- **[LOW]** `OPENSVN_REQUIRE_SVN=""` is an error. That is good, but a CI `${{ }}` expression that evaluates to an empty string will fail loudly, so set it to a literal `1` in workflows.
- **[LOW]** Because of the lead rule, a CI runner with an svn that is too old skips silently apart from stderr. dev-01a's ci.yml step should assert `svn --version` ≥ 1.15 on Linux/macOS, so the skip path cannot hide a broken install.

API sufficiency: `resolve(MinVersion)`, `svn()`, `svnadmin()`, `version()` and `config_dir()` cover the existing call sites in #29/#33/#35 (create, checkout, propset, status, info). Adopt now; add `file_url` in a small follow-up.

Status: DONE
Summary: PR #50 addresses all five N2 points and follows the lead's too-old-skips rule; crate-local gates pass with the real svn 1.15.0. APPROVE; a Windows-safe `file_url` helper is recommended before adoption spreads.

---

# PR #60 review: legacy svn 1.8.19 / 1.9.12 / 1.10.8 matrix, head `1b00c10`

**Verdict: REQUEST CHANGES** (one small CI fix; everything else checks out).

Verified locally (isolated project `opensvn-compat-rv`, renamed ports, `CARGO_BUILD_JOBS=2`, one image built; cleaned afterwards):
- **SHA-512 pins**: 1.8.19 `dc163b6d…`, 1.9.12 `08a5c6c0…` and 1.10.8 `94464202…` match `archive.apache.org`. Base images are still the pinned `bookworm-slim@sha256:7c7b2c96…`, which the legacy images share via the same Dockerfiles.
- **Image build**: rebuilt `svn18-server` alone from this SHA. It compiled successfully with `SVN_CONFIGURE_EXTRA="svn_cv_pycfmt_apr_int64_t=L"`, ran `sha512sum -c`, and installed `mod_dav_svn.so`.
- **Gates**: fmt clean; clippy `-D warnings` clean; 26 unit tests pass.
- **E2E**: `up --no-build --legacy` and `fixtures --legacy` give the main pin on 1.14, 1.15, 1.10 and 1.9; 1.8 matches its own pin `8d2d1cbd…` / `1b33aedb…`. `run` passes 4/4 (default set; legacy excluded). `run --legacy` passes 10/10: 3 self-checks, 3 smokes and the 4 default scenarios.
- **Is the separate 1.8 pin a weakening?** No. I loaded the 1.8 server's `basic` dump into a fresh 1.15 repo and re-dumped it: sha256 = `314b86a3…`, identical to the main pin. So the 1.8 repo has the same content, and only the dump serialisation differs. The 1.14/1.15 pins are unchanged.
- **`sort_entry_runs`** (`compare.rs`) sorts `<entry>…</entry>` blocks for every reader, not just 1.8. For 1.14/1.15 this only drops entry *order*, and order is a property of the reader binary (both sides are read by the same `reader`), not of the working copy. Content inside each block is still compared, and the unit test proves that a changed field is detected. `<logentry>` is unaffected. Acceptable, though gating it on `reader ∈ {svn18}` would be tidier (LOW).

## IMPORTANT
- **I1. The legacy images are not opt-in for the PR job.** `.github/workflows/compat.yml` PR job → `docker buildx bake -f compose.yml`. Bake **ignores compose `profiles`**: `--print` lists all 11 targets with and without `COMPOSE_PROFILES` (verified). So every PR builds the 1.8/1.9/1.10 server and client images as well: 6 extra svn compiles at `SVN_MAKE_JOBS=2`, which risks the 45-minute timeout on a cold cache.
  Fix: in the PR job's bake step, add `targets: certs,svn14-server,svn14-client,svn15-server,svn15-client` (or a bake group), and give the legacy job the legacy targets plus `svn15-client`.
  Also consider `*.args.SVN_MAKE_JOBS=4` on CI runners; the new default of 2 replaces `nproc` for the main images too.

## MODERATE
- **Smoke coverage of the WC-created-by-1.15 → old-client direction**: covered for reading. `smoke-svn1x` checks out wc `c` with svn15, then runs `svn status` + `svn update` with the old client, and compares `a` vs `c` on tree and status.
  Missing: (1) an old-client **commit** from a 1.15-created WC; (2) `info`/`wcdb` on the `a`/`c` comparison; (3) a negative case: a format-32 WC (`svn checkout --compatible-version 1.15`) must be refused cleanly by 1.8–1.14 (DoD l.129, "format 32 readable").
- `xtask compose()` now always passes `--profile legacy`. That is harmless, since `up` names services explicitly and `down -v` should remove legacy volumes anyway, but `status` will list legacy containers too.
- The nightly legacy job writes the shared `compat-images` cache scope (scheduled runs use the main ref, so this is fine), but its cold run will be slow until the cache is populated.

Status: DONE_WITH_CONCERNS
Summary: Pins and the 1.8 build path were verified; the 1.8 dump is content-identical to the main pin; legacy E2E passes 10/10. REQUEST CHANGES only because bake ignores compose profiles, so the PR compat job builds the legacy images too. Restrict the bake targets.
