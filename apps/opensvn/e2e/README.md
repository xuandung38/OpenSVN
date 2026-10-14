# End-to-end flows

`flows.test.mjs` drives the real OpenSVN binary (built with `--features real-engine`) through
[tauri-driver](https://v2.tauri.app/develop/tests/webdriver/) and WebKitWebDriver, against
Apache + mod_dav_svn over HTTPS with Basic auth. It covers the MVP flows of
`docs/system-design/01` section 10:

| Test | Flow |
|---|---|
| 10.1 first use | Welcome → Checkout → URL → trust the certificate → sign in → empty Changes |
| 10.2 daily commit | edit → see the diff → Ctrl+Enter commit → list empties |
| 10.2 out of date | commit refused → Update merges the incoming change → commit goes through |
| 10.3 update with a conflict | conflict banner → resolver → "Mine" → banner gone, marker files removed |
| 10.4 needs-lock file | read-only file listed under Locks → Lock to edit… → writable → Unlock… → read-only → lock → edit → commit releases the lock |
| 10.4 lock held elsewhere | Lock to edit… → refused: owner and comment shown → Steal + confirmation → writable, the other lock is gone |
| 10.5 switch | Location → Switch… → branch URL; local change carried over, toolbar updates |
| history and blame | History tab lists the revisions; Browse → file → Blame attributes lines |

Every test gets a fresh scratch repository (`e2e-<test>-<pid>-<n>`) and a fresh app profile
(`HOME`, `XDG_*_HOME` in a temp dir), so tests never share state. On failure a screenshot, the
window text and the driver log land in `e2e-artifacts/` (`OPENSVN_E2E_ARTIFACTS`).

## Requirements (Linux)

- Tauri build dependencies (`libwebkit2gtk-4.1-dev` …), Node 24 and pnpm.
- `WebKitWebDriver` (Debian/Ubuntu: `webkit2gtk-driver`), `xvfb-run` (`xvfb`) when there is no
  display, and the `svn` command line client (seeds repositories and checks results).
- `cargo install tauri-driver --locked`.
- A server, one of:
  - the compat container: `cargo xtask compat up` (svn 1.15 on `https://localhost:8444/svn`;
    repositories are created with `docker exec`);
  - your own Apache: set `OPENSVN_COMPAT_LOCAL_URL` (the `SVNParentPath` URL, e.g.
    `https://localhost:18276/svn`) and `OPENSVN_COMPAT_LOCAL_REPOS` (that directory, writable by
    you and readable/writable by Apache, e.g. with a default ACL for `www-data`), plus `svnadmin`.

The server must accept `tester` / `tester-pass` (Basic). Its certificate may be self-signed: the
test accepts it in the app's trust dialog, and `svn` runs with
`--trust-server-cert-failures=unknown-ca,cn-mismatch`. The server certificate itself must not be a
CA (`basicConstraints=CA:FALSE`): rustls refuses a CA certificate used as the server's
(`CaUsedAsEndEntity`). `scripts/test-env/visualsvn-like.sh up` sets up a suitable Apache.

## Running

```sh
cd apps/opensvn
pnpm e2e                                    # build (debug, real engine) + all flows
OPENSVN_E2E_SKIP_BUILD=1 pnpm e2e -- --test-name-pattern="10.4"
```

`OPENSVN_E2E_APP` points at another binary (no build). `TAURI_DRIVER` overrides the driver path.

## Writing tests

Find controls the way a user does: `app.click(role, name)`, `app.fill(label, text)`,
`app.contextMenu(role, name)`, `app.waitText(text)`. Names are the accessible names (aria-label,
label, or visible text; a string matches exactly, a RegExp loosely). Wait for an outcome
(`waitFor`, `waitText`) instead of sleeping, and check results on disk or with `repo.svn(...)`.
