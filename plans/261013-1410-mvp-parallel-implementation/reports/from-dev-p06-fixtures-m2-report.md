# P06 M2: WC fixtures, protocol captures, evil-server fixtures

Branch `feat/p06-fixtures-m2` (from `origin/main` 7e418f6). Draft PR.

## Delivered

1. **WC state fixtures** (`fixtures/wc/`): produced by `make-wc-fixtures.sh <31|32>` with real svn.
   - `f31/`: 28 states, svn **1.14.3** (Ubuntu 24.04 package; the pinned 1.14.5 image could not be
     used because the sandbox has no Docker daemon). The list of states is in `tests/compat/README.md`.
   - `f32/`: 11 states, svn **1.15.0**, built here from the SHA-512-verified tarball (same pin as
     `docker/compose.yml`), file:// only (built without serf). It uses `--compatible-version 1.15`,
     plus one `--store-pristine no` state.
   - Repo = `make-basic.sh` (hash check passes with 1.14.3) + r11..r14 for the conflict states,
     at the fixed path `file:///tmp/opensvn-wc-fixtures/repos/basic`.
   - `<state>.status.txt` is byte-identical across two runs. The tarballs are not byte-identical
     (wc.db mtimes and lock times), and this is documented.
   - `tests/compat/tests/wc_fixtures.rs` unpacks every tarball and checks: the wc.db format, the
     state-specific facts (conflict rows, LOCK row, moved_to, depth, excluded, changelist,
     switched repos_path, mixed revisions, EXTERNALS, working nodes, base-deleted), an empty
     WORK_QUEUE, and that `wcdb_dump::dump_wc_db` reads it. If zstd/tar are missing the test
     skips, unless `OPENSVN_REQUIRE_FIXTURE_TOOLS=1`.
2. **Protocol captures** (`fixtures/protocol/{checkout,checkout-skelta,update,log,commit}/`):
   `capture.sh` runs native Apache 2.4.58 + mod_dav_svn 1.14.3 behind
   `crates/svn-protocol/tools/capture/proxy.py`. `normalize.py` anonymises the output (host is
   svn.test.local, no Authorization, pinned Date/Server, commit timestamps pinned).
   `crates/svn-ra/tests/fixture_replay.rs` replays every session through `HttpTransport` against
   a scripted HTTPS fake server. The fake emulates Basic auth and matches by method+path. Each
   answer is parsed with svn-protocol: Capabilities/UrlBuilder, update-report, log-report,
   multistatus, create-txn, MERGE. All 5 sessions replay fully.
   Content is reproducible but not byte order: APR hash order of props/attributes, plus serf
   request concurrency.
3. **Evil-server fixtures** (`fixtures/evil/`, 70 cases, `generate.py`, README with each attack):
   - `svn-protocol/tests/evil_server.rs`: update/propfind/log/merge/locks/OPTIONS. Each case is
     tested whole and fed byte-by-byte.
   - `svn-delta/tests/evil_svndiff.rs`: 19 svndiff cases, raw and base64.
   - `svn-ra/tests/evil_transport.rs`: 10 redirect variants × 4 methods, a 1 GiB body against a
     1 MiB limit, a stalled body/read timeout, certificates for another name (trusted and
     untrusted CA, AcceptAlways prompter ⇒ still TlsUntrusted and no prompt), and all update
     cases served over HTTPS.

## Findings (production code not changed)

- **Gap, svn-protocol `UrlBuilder::new`**: `SVN-Repository-Root` is not checked to be an
  ancestor of the session URL (`/elsewhere` is accepted; `/svn/repo/..` normalises to `/svn` and
  is accepted). The test proves every built URL stays same-origin, so there is no cross-host
  leak. However, svn-ra path math assumes ancestry. I did not fix it: a strict check needs a
  case-insensitive fallback for VisualSVN repo names, which is a design call for the P03 owner.
  The cases are marked `gap-same-origin`; flip them to `reject` once fixed.
- Everything else was rejected safely: all traversal, Windows drive/UNC, ADS, DOS-device and
  admin-dir names, foreign/traversing hrefs, XML bombs, and all svndiff bombs. The zlib bomb is
  rejected without inflating.

## Shared-file edits

- `crates/svn-ra/Cargo.toml`: adds the `svn-protocol` dev-dependency. PR #75 adds it as a normal
  dependency. Keep #75's line on merge.
- `fixtures/check-no-real-hosts.sh`: XML namespace URIs (tigris/apache.org/w3.org) are dropped
  before the host check. `:$VAR`/`{x}` ports are allowed. I checked that a real host is still caught.
- `tests/compat/README.md`: new "Committed fixture sets (M2)" section.

## Gates (local, this branch)

- `cargo fmt --all --check`: pass.
- `cargo clippy -p svn-protocol -p svn-delta -p svn-ra -p opensvn-compat --all-targets -- -D warnings`:
  pass. This is scoped to the touched crates, not the whole workspace, because the shared sandbox
  disk was nearly full. The app/tauri crates were not touched.
- `cargo test --no-fail-fast -p svn-protocol -p svn-delta -p svn-ra -p opensvn-compat`:
  44 suites, **352 passed, 0 failed, 8 ignored** (Docker-only).
- `fixtures/check-no-real-hosts.sh`: clean.
- Flaky test outside this change: `svn-ra transport_auth::cancel_interrupts_a_request_queued_behind_a_prompt`
  failed in 3 of 5 isolated runs (`credential_prompts()` was 2, expected 1). The queued request
  also prompts after the owner is cancelled, or the timing is too tight. This PR touches no
  svn-ra source, so it belongs to the P04 owner.
- No FE gates: nothing under apps/ changed.

## Not done

- No 1.14.5-generated f31 set, and no Docker-produced captures: no Docker daemon in the
  sandbox. Regenerate with the pinned images:
  `SVN=<svn14-client> fixtures/wc/make-wc-fixtures.sh 31`.
- Not captured yet: lock/unlock, get-locks, switch, status -u, file-revs. The older per-exchange
  captures in `crates/svn-protocol/tests/data` already cover them.
- `docker/evil-server/` container and the hostile repository (`<script>` names/props, SVG,
  symlink escape) are not built.
- `xtask compat fixtures` does not drive the new generators yet.
