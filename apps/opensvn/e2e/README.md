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
| 10.5 switch | Branch ▾ → feature-x → "Carry changes and switch"; local change carried over, toolbar shows the branch; Switch… by URL back to trunk |
| history and blame | History tab lists the revisions; Browse → file → Blame attributes lines |

`settings.test.mjs` needs no server (a fresh profile starts on the Welcome screen, like a fresh
install): the toolbar gear, Ctrl+, and Cmd+, (page reporting macOS) open a visible Settings sheet,
Tauri accepts the menu event names the UI subscribes to, and the Network page refuses an
out-of-range connection count inline, then saves a valid one through the real backend. `pnpm e2e` runs only the flows; run
this one after the same build with `xvfb-run -a node e2e/settings.test.mjs` (tauri-driver and
WebKitWebDriver on `PATH`).

`hostile.test.mjs` (#18) checks out the hostile repository of `fixtures/evil/make-hostile-repo.sh`
from the evil server (`docker/evil-server/run-local.sh`, started by the test itself on a free
port, no Docker) and renders its log (History, revision details), its file list (Browse, with
previews of the SVG/HTML/symlink files) and its properties (a local property edit in the diff
view). Traps installed in the page record every dialog, `window.open`, inserted script/frame/
handler/`javascript:` node and fetched resource; a local beacon listener (the repository's
`BEACON`) and stand-ins for `xdg-open`/`gio`/file managers on `PATH` record any request or launch.
All must stay empty. The symlinks that leave the working copy are refused by "Show in file
manager" and "Open with default app", and by `reveal_in_os` / `open_external` invoked straight
from the page; `wc_add_existing` refuses forged grants (PathGrant). It needs Apache with mod_dav_svn
(`apache2 libapache2-mod-svn`) and `svn`/`svnadmin`/`svnmucc` on `PATH`:
`pnpm e2e:hostile` (or `OPENSVN_E2E_SKIP_BUILD=1 pnpm e2e:hostile`).

`resume.test.mjs` checks out through `scripts/test-env/flaky-tls-proxy.py` in outage mode: the
network goes silent mid-checkout, the checkout dialog shows "Connection lost, retrying in Ns…"
with "Retry now", and the checkout finishes on its own once the network is back. It needs the
server certificate and key of `scripts/test-env/visualsvn-like.sh` (next to
`OPENSVN_COMPAT_LOCAL_CA`, or in `OPENSVN_E2E_PROXY_CERTS`); run it after the same build with
`xvfb-run -a node --test-timeout=600000 e2e/resume.test.mjs`.

### Reliability: crash, memory, soak (#20, #21)

They share `reliability.mjs`. Each one starts a private D-Bus session with an unlocked
gnome-keyring (`dbus-launch`, `gnome-keyring-daemon`; Debian/Ubuntu: `dbus-x11 gnome-keyring`) so
the app keeps the password across a relaunch, as on a desktop. The server must be your own Apache
(`OPENSVN_COMPAT_LOCAL_*`, e.g. `scripts/test-env/visualsvn-like.sh up` with
`OPENSVN_COMPAT_LOCAL_URL=https://svn.opensvn.test:18443/svn-basic`): the memory tests write the
large repository straight into `OPENSVN_COMPAT_LOCAL_REPOS`. `OPENSVN_SVN_BIN` picks the oracle
client (svn 1.15).

| Test | Command | What |
|---|---|---|
| `crash.test.mjs` | `pnpm e2e:crash` (debug build, `--features failpoints`) | commit and update on the real app, SIGKILL of the app's whole process group at a random moment (`OPENSVN_CRASH_SEED`) or when a failpoint is reached; relaunch on the same profile: no "Working copy is locked"/"Run cleanup" banner, `svn status` clean or only the local edits, the commit landed at most once, the update finishes and merges |
| `rss.test.mjs` | `pnpm e2e:rss` (release build) | 100k-file working copy: checkout, status (cold reopen), refresh, a 5k-file watcher burst, History, update of 20 revisions; peak RSS of the app process tree < 400 MB |
| `soak.test.mjs` | `pnpm e2e:soak` (release build) | watcher and poller on a 50k-file working copy, edits every 15 s, a commit elsewhere every minute, an update every 2 minutes, for `OPENSVN_SOAK_MINUTES` (10 nightly, 120 weekly); no RSS creep |

The failpoint build arms `OPENSVN_CRASH_FAILPOINT=<name>[:<hit>]` at start-up
(`src-tauri/src/crash_hook.rs`, `failpoints` builds only): at that hit the app writes
`OPENSVN_CRASH_MARKER` and stops there until it is killed. `OPENSVN_CRASH_ITERS` sets the kills
per operation (default 4), `OPENSVN_CRASH_PLAN=<failpoint>:<hit>` replays one failpoint case. A
markdown report lands in `e2e-artifacts/` (`crash-report.md`, `rss-report.md`, `soak-report.md`).
`OPENSVN_SOAK_APP_ENV="MALLOC_ARENA_MAX=2"` (or an `LD_PRELOAD` of jemalloc) compares allocators.

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
