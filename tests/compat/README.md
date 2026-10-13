# OpenSVN compat harness

Proves "behaves like the real svn". Dockerised Apache + `mod_dav_svn` servers and svn CLI clients,
pinned to **Subversion 1.14.5** and **1.15.0** (built from the official source tarballs, SHA-512
verified), plus a scenario runner that compares resulting working copies.

```
docker/svn-server/   Apache httpd 2.4 + mod_dav_svn, HTTPS (generated test CA), Basic auth
docker/svn-client/   svn CLI image (also hosts gen-certs.sh runs)
docker/compose.yml   svn14-server :8443, svn15-server :8444, svn14-client, svn15-client
fixtures/repos/      deterministic repo generators (+ expected/ dump hashes)
xtask/               cargo xtask compat ...
tests/compat/        this crate: scenario DSL, runner, comparators, wc.db dump
```

## Run locally (macOS: Docker Desktop or OrbStack; Linux: Docker Engine + compose v2)

```sh
cargo xtask compat up           # build images (first run ~10+ min, compiles svn), start servers
cargo xtask compat fixtures     # create + hash-verify fixture repos on both servers
cargo xtask compat run          # run tests/compat/scenarios/harness (svn vs svn self-check)
cargo test -p opensvn-compat -- --ignored   # Docker-backed test wrapper around the same thing
cargo xtask compat status
cargo xtask compat down [--purge]
```

- `up --no-build` reuses existing images. Servers answer in under 30 s once images exist.
- Servers: `https://localhost:8443/svn/` (1.14.5), `https://localhost:8444/svn/` (1.15.0). Inside
  the compose network they are `svn14.test.local` / `svn15.test.local`.
- Test credentials: user `tester`, password `tester-pass` (test-only, baked into the image).
- Test CA: `target/compat/certs/ca.crt`. Trust it explicitly, e.g.
  `svn info --config-option servers:global:ssl-authority-files=target/compat/certs/ca.crt https://localhost:8444/svn/basic`.
- Request capture: `SVN_DUMPIO=1 cargo xtask compat up` turns on `mod_dumpio`; bodies land in
  `target/compat/logs/svn1x/error.log`, request lines in `access.log`.
- Unit tests (`cargo test -p opensvn-compat`) need no Docker.

## Older Subversion versions (1.8 to 1.10)

Servers (mod_dav_svn) and clients for **1.10.8, 1.9.12 and 1.8.19** live in the compose `legacy`
profile; they are opt-in to keep the default environment small. Format-31 working copies written by
1.8+ clients must stay readable by the engine, and by old clients.

```sh
cargo xtask compat up --legacy        # also starts svn110/svn19/svn18 servers + clients
cargo xtask compat fixtures --legacy  # fixture repos on every server (dump hashes verified)
cargo xtask compat run --legacy       # default scenarios plus scenarios/harness/legacy/
```

- Full scenario set: 1.14.5 and 1.15.0. Per old version only `selfcheck-<tag>` (the image works: two
  same-version checkouts compare equal on tree/status/info/wcdb) and `smoke-<tag>` (checkout,
  update, commit, status -u, log, lock/unlock, and a working copy created by svn 1.15 that the old
  client must `status` and `update`; tree/status compared). Nightly may run everything.
- Servers: `svn110.test.local:8447`, `svn19.test.local:8446`, `svn18.test.local:8445` (host ports on
  127.0.0.1). Tags in scenarios: `svn110`, `svn19`, `svn18`.
- Build notes: 1.8/1.9 need `svn_cv_pycfmt_apr_int64_t=L` passed to `./configure` (the Python-binding
  probe cannot run against APR 1.7) and have no `--with-lz4`; the image build is serial and uses
  `make -j2` (override with `SVN_MAKE_JOBS`). Cold compile is about 7-9 minutes per image.
- `svnadmin` 1.8 writes dump headers in a different order, so `fixtures/repos/expected/*.1.8.sha256`
  pin its (equivalent) dump separately. 1.8 and 1.9 also cannot read a revprop value from a pipe,
  so fixtures use temp files.
- `info --xml --recursive` lists entries in per-process hash order on 1.8, so the normaliser sorts
  `<entry>` blocks. `dav_cache` differs between a committed and an updated working copy, so smoke
  scenarios do not compare `wcdb` across those; `selfcheck-<tag>` does on identical operations.

## Fixture repos

| Repo | Script | Contents |
|---|---|---|
| `basic` | `fixtures/repos/make-basic.sh` | trunk/branches/tags; LF, CRLF, mixed-EOL text; seeded binary; `svn:eol-style`, `keywords`, `needs-lock`, `mime-type`, `executable`, `ignore`, `externals`; rename and copy history; a branch and a tag |
| `nolayout` | `fixtures/repos/make-nolayout.sh` | no trunk/branches/tags; Vietnamese (NFC) folder/file names; names with spaces |
| `large` | `fixtures/repos/make-large.sh` | N files (default 100000), on demand: `cargo xtask compat fixtures --large [N]`; never committed |

Generators pin UUID, author, per-revision dates and seeded binary data; the SHA-256 of
`svnadmin dump` is checked against `fixtures/repos/expected/*.sha256` (same on 1.14.5 and 1.15.0).
After an intentional content change: `cargo xtask compat fixtures --update-hash`.
`fixtures/check-no-real-hosts.sh` (run in CI) rejects real hosts and credentials in fixtures/scenarios.

## Scenarios

YAML files; see the header of `src/scenario.rs` for the schema.

```yaml
name: my-scenario            # [A-Za-z0-9_-], unique; also the work dir name
server: svn15                # svn14 | svn15 (default svn15)
repo: basic                  # fixture repo
fresh_repo: true             # hotcopy it first if the scenario commits
steps:
  - { on: svn15, wc: a, svn: [checkout, "{url}/trunk", "."] }
  - { on: svn15, wc: a, sh: "echo x >> README.txt" }
  - { on: engine, wc: b, svn: [checkout, "{url}/trunk", "."] }     # needs an EngineDriver
  - { on: tortoise-manual, wc: c, note: "Update in TortoiseSVN, then press Enter" }
compare:
  - { left: a, right: b, checks: [tree, status, info, wcdb], reader: svn15 }   # reader defaults to svn15
expect: match                # `mismatch` = negative control: every listed check must find a difference
```

`{url}`, `{repo}`, `{wc}` are substituted. svn steps get `--username/--password/--non-interactive`
appended. Comparisons:
- `tree`: paths, bytes, exec bit and read-only state (`svn:needs-lock`); `.svn` excluded.
- `status` / `info`: `svn ... --xml` read with `reader` (default svn15), attributes sorted (svn
  emits them in per-process hash order), `text-updated`/`prop-updated` dropped, wc path replaced
  by `{WC}`. Server dates, lock data and revisions stay compared.
- `wcdb`: `user_version`, every index/trigger/view, all rows sorted. Only `NODES.last_mod_time`
  is masked (SET/NULL) and skel property columns are canonicalised (svn writes them in hash
  order). A non-empty `WORK_QUEUE` after the steps is a failure.

`selfcheck-must-fail.yaml` is the negative control (`expect: mismatch`): it proves each comparator
notices a real difference. Run one scenario run at a time (work dir and ports are shared).

Run one: `cargo xtask compat run tests/compat/scenarios/wc/my-scenario.yaml` (files or dirs).

### Where scenarios live (one directory per owner)

| Directory | Owner |
|---|---|
| `scenarios/harness/` | P06 (self-checks) |
| `scenarios/protocol/` | P03 |
| `scenarios/wc/` | 07A / 07B |
| `scenarios/ra/read/`, `scenarios/ra/write/` | 10a, 10b |
| `scenarios/client/` | 11U, 11C |
| `scenarios/e2e/` | 12b, 15 |

Windows E2E cannot use Docker Linux on hosted runners: use an internal VisualSVN test server or a
Linux server reachable over the network.

### Native svn oracle for unit tests in other crates

For fast per-crate tests that only need a local `svn`/`svnadmin` (file:// repositories), use
`opensvn_compat::oracle` (add `opensvn-compat` as a dev-dependency) instead of calling
`Command::new("svn")`:

```rust
use opensvn_compat::oracle::{MinVersion, Oracle};
let Some(oracle) = Oracle::resolve(MinVersion::V1_15)? else { return Ok(()) }; // skipped
oracle.svnadmin().args(["create", path]).status()?;
oracle.svn().args(["checkout", url, wc]).status()?;
```

Environment: `OPENSVN_SVN_BIN` / `OPENSVN_SVNADMIN_BIN` (default `svn` / `svnadmin` on PATH);
`OPENSVN_REQUIRE_SVN=1` turns a missing or too-old binary into `Err` instead of a skip (CI sets it).
`MinVersion::V1_15` is the default; ask for `V1_14` only for 1.14-specific behaviour. The same
crate exports the normalisers: `wcdb_dump::dump_sqlite` / `dump_wc_db`, `compare::snapshot_tree` /
`diff_trees`, `skel::canonical`.

### Engine steps

`on: engine` steps are executed by an `EngineDriver` (trait in `runner.rs`) that the owning crate's
test supplies via `Runner::with_engine`; `compat-run` has no engine attached and fails such
scenarios with a clear error.

## Capturing new fixtures

1. `SVN_DUMPIO=1 cargo xtask compat up`, run the operation with the svn CLI (use `{url}` over HTTPS).
2. Take request/response bodies from `target/compat/logs/svn1x/error.log`.
3. Anonymise before committing: drop `Authorization`/`Cookie` headers, rewrite hosts to
   `svn.test.local`; `fixtures/check-no-real-hosts.sh` must pass.
4. Protocol capture set and WC state fixtures are produced in the next milestone.

## Environment verification (to be filled by the lead)

Oracle is 1.14.5 unless production already runs 1.15.

| Item | Version | Source / date |
|---|---|---|
| Internal VisualSVN Server `mod_dav_svn` | TODO | |
| TortoiseSVN in use | TODO | |
| Oracle decision (1.14.5 vs 1.15.0) | TODO | |
