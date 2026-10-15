# VisualSVN-like test environment (Linux)

A local stand-in for an internal VisualSVN Server (Apache + mod_dav_svn on Windows, Active
Directory sign-in through Integrated Windows Authentication or Basic over HTTPS), to find
client bugs before trying OpenSVN against the real server. One script builds it:
`scripts/test-env/visualsvn-like.sh`.

## What it emulates

| VisualSVN Server | Here |
| --- | --- |
| Active Directory (Kerberos KDC) | MIT krb5 KDC, realm `OPENSVN.TEST`, users `alice` / `alice-pass`, `bob` / `bob-pass` |
| Active Directory domain controller | `VSVN_KDC=samba`: Samba AD DC, realm `AD.OPENSVN.TEST`, same users as domain accounts, AES only (see below) |
| Service principal `HTTP/<fqdn>` | `HTTP/svn.opensvn.test`, keytab for Apache |
| Integrated Windows Authentication + Basic | `/svn`: `mod_auth_gssapi`, Negotiate first, Basic checked against the KDC (`GssapiBasicAuth On`) |
| Integrated Windows Authentication only | `/svn-sso`: Negotiate only |
| Subversion authentication | `/svn-basic`: htpasswd Basic (`tester` / `tester-pass`, `bob` / `bob-pass`) |
| Repositories root `https://host/svn/` | `SVNParentPath` + `SVNListParentPath On`; `/svn` serves the XML index with `SVNIndexXSLT` like VisualSVN |
| VisualSVN authz (`[groups]`, global and per-repository rules) | `AuthzSVNAccessFile` (`$VSVN_DIR/authz`): `developers` rw, `readers` (bob) r, `MyRepo:/secret` denied to readers |
| NTFS (case-insensitive repository names) | `myrepo` is a second name (symlink) of `MyRepo` |
| Repository names with spaces / Vietnamese | `My Project`, `Dự án Tiếng Việt` |
| Pre-commit hook policy | `Hooked`: rejects log messages without `ISSUE-<n>` (English + Vietnamese message) |
| Plain http with IWA | `http://svn.opensvn.test:18080/svn` offers Negotiate only |
| Server versions | distribution mod_dav_svn (Ubuntu 24.04: 1.14.3) on `:18443`, self-built 1.15.0 on `:18444` (when `/opt/svn115` exists) |

Not emulated: NTLM (OpenSVN refuses it by design), Extended Protection: `mod_auth_gssapi`
1.6.4 has no channel-binding option, so the bindings the client sends are accepted but not
checked (the GSSAPI round trip with bindings is covered by
`crates/svn-ra/tests/negotiate_gssapi_kdc.rs`), Windows file locking, VisualSVN's own web UI.

## Set up

Debian/Ubuntu, as root (container/VM) or with sudo (the script adds the host name to
`/etc/hosts`):

```sh
scripts/test-env/visualsvn-like.sh install   # apt packages (KDC, Apache, mod_auth_gssapi, mod_dav_svn, krb5 headers)
scripts/test-env/visualsvn-like.sh up        # realm, CA, repositories, KDC + Apache, kinit alice
eval "$(scripts/test-env/visualsvn-like.sh env)"
klist                                         # alice@OPENSVN.TEST
curl --negotiate -u : --cacert "$OPENSVN_COMPAT_LOCAL_CA" https://svn.opensvn.test:18443/svn/
```

State lives in `$VSVN_DIR` (default `/tmp/opensvn-visualsvn`): `krb5.conf`, the KDC database,
`http.keytab`, `certs/ca.crt`, `repos/`, `authz`, `logs/` (Apache access/error logs with
`auth_gssapi:debug`; `krb5kdc/kdc.log`). `down` stops the servers, `purge` deletes everything.
Ports and names can be changed with `VSVN_HOST`, `VSVN_HTTPS_PORT`, `VSVN_HTTP_PORT`,
`VSVN_HTTPS115_PORT`, `VSVN_KDC_PORT`, `VSVN_REALM`.

Notes:

- Apache never calls `setlocale()`, so mod_dav_svn cannot list or open repositories with
  non-ASCII names under a plain Apache on Linux ("Error converting entry in directory ... to
  UTF-8"). The script preloads a two-line library that sets the UTF-8 locale. VisualSVN on
  Windows has no such problem.
- As root, Apache runs as `www-data`; repositories must then be created through
  `$VSVN_DIR/bin/svnadmin` (exported as `OPENSVN_COMPAT_LOCAL_SVNADMIN`).
- `krb5.conf` sets `dns_canonicalize_hostname = false` and `rdns = false`, so the service name is
  exactly the URL host (see docs/visualsvn-iwa-checklist.md).

### mod_dav_svn 1.15.0

Build it once (about 5 minutes on 4 cores; needs `apache2-dev libapr1-dev libaprutil1-dev
libsqlite3-dev libexpat1-dev zlib1g-dev`):

```sh
curl -fsSLO https://archive.apache.org/dist/subversion/subversion-1.15.0.tar.bz2
echo "6bb79686baab7176e9b6c519b23141f66694298c0759d1e12332fef329eaa632e4f2e65c75ae0cd05e48e16c23d7eda7c59b322bdf03707e1a46025673282ec2  subversion-1.15.0.tar.bz2" | sha512sum -c -
tar xjf subversion-1.15.0.tar.bz2 && cd subversion-1.15.0
./configure --prefix=/opt/svn115 --with-apxs=/usr/bin/apxs --with-apache-libexecdir=/opt/svn115/libexec \
  --without-serf --with-lz4=internal --with-utf8proc=internal --disable-mod-activation \
  --without-berkeley-db --without-swig --disable-nls
make -j4 && make install
```

`up` then also starts a second Apache with that module on `:18444` (same repositories, same
locations).

### Samba AD DC variant

`VSVN_KDC=samba` replaces the MIT KDC with a real Active Directory domain controller (Samba 4.19,
Ubuntu 24.04). Apache, mod_auth_gssapi, both mod_dav_svn versions, the repositories and the test
suites stay the same; state lives in `/tmp/opensvn-visualsvn-ad`.

```sh
VSVN_KDC=samba scripts/test-env/visualsvn-like.sh install   # adds samba-ad-dc, samba-ad-provision, python3-samba, ldb-tools
VSVN_KDC=samba scripts/test-env/visualsvn-like.sh up
eval "$(VSVN_KDC=samba scripts/test-env/visualsvn-like.sh env)"
klist -e        # alice@AD.OPENSVN.TEST, Etype aes256-cts-hmac-sha1-96
```

What it adds over the MIT realm, as an AD domain does it:

- `samba-tool domain provision` (realm `AD.OPENSVN.TEST`, NetBIOS domain `OPENSVNAD`, no DNS
  server, loopback only); `alice` and `bob` are domain users (`sAMAccountName`, UPN
  `alice@ad.opensvn.test`), `alice` is in the group `SVN Developers`.
- Tickets carry a PAC: the Negotiate header of a request is about 2.8 KB instead of a few hundred
  bytes.
- Apache's identity is a service account, `svc-svn`, with the SPN `HTTP/svn.opensvn.test` and
  `msDS-SupportedEncryptionTypes = 24`; its keytab comes from `samba-tool domain exportkeytab`
  (AES256 and AES128 keys only).
- AES only: the KDC never issues RC4 (`kdc supported enctypes = 24`) and `krb5.conf` permits only
  the two AES enctypes, as on a domain with RC4 switched off.
- Basic on `/svn` (`GssapiBasicAuth On`) checks the password against the domain controller.

Limits:

- Samba has no setting for the KDC port: it listens on `127.0.0.1:88`. `up` needs root, and no
  other KDC may use port 88.
- Ubuntu's samba-tool needs the Python the bindings were built for (3.12); the script finds it when
  `python3` is a newer version.
- No DNS-based KDC discovery and no cross-realm trust: `krb5.conf` names the KDC.

### Secret Service (Linux keychain)

`scripts/test-env/secret-service-roundtrip.sh` runs secure-store's `os_store_roundtrip` (set, get,
delete through the Secret Service API, the way the app stores passwords on Linux) without a
desktop: a private bus from `dbus-run-session`, and `gnome-keyring-daemon --unlock` creating and
unlocking a login keyring with a throwaway password in a temporary `HOME`. The user's own keyring
is never touched. `install` adds `dbus`, `dbus-x11` and `gnome-keyring`; the nightly workflow runs
it.

```sh
scripts/test-env/secret-service-roundtrip.sh install
scripts/test-env/secret-service-roundtrip.sh --locked
```

## Test suites

All need the environment of `env`. Build with `negotiate-gssapi` (krb5 headers and libclang):

```sh
F=real-engine,negotiate-gssapi,engine-conformance

# The engine conformance suite with Kerberos SSO; fails if any credential prompt is shown.
OPENSVN_COMPAT_AUTH=negotiate OPENSVN_COMPAT_LOCAL_URL=https://svn.opensvn.test:18443/svn-sso \
  cargo test -p opensvn --features $F --test real_engine_dav -- --ignored
# Same with Negotiate + Basic offered, and against 1.15.0:
OPENSVN_COMPAT_AUTH=negotiate OPENSVN_COMPAT_LOCAL_URL=https://svn.opensvn.test:18443/svn ...
OPENSVN_COMPAT_AUTH=negotiate OPENSVN_COMPAT_LOCAL_URL=https://svn.opensvn.test:18444/svn-sso ...
# Basic (htpasswd):
OPENSVN_COMPAT_LOCAL_URL=https://svn.opensvn.test:18443/svn-basic \
  cargo test -p opensvn --features $F --test real_engine_dav -- --ignored

# VisualSVN layout, SSO fallbacks, authz, hooks, keywords/eol, needs-lock (port 18443 or 18444):
OPENSVN_VSVN_HTTPS_PORT=18443 cargo test -p opensvn --features $F --test visualsvn_like -- --ignored --skip big_
# 100 MB+ file and 10k files (slow; sizes: OPENSVN_VSVN_BIG_MB, OPENSVN_VSVN_MANY_FILES):
cargo test -p opensvn --features $F --test visualsvn_like -- --ignored big_ --test-threads=1 --nocapture
```

The no-ticket cases re-run themselves in a child process with an empty credential cache, so a
ticket in `KRB5CCNAME` is fine for the whole run.

### Connections closed, reset or cut off by the server

Apache on Windows, IIS and many proxies end TLS without a `close_notify`. They also close or reset
idle keep-alive connections and enforce `KeepAliveTimeout` / `MaxKeepAliveRequests`. Two tools
reproduce this:

- **Low keep-alive limits** on both Apaches. They apply when `up` writes the configuration, so
  restart with `down` first:

  ```sh
  scripts/test-env/visualsvn-like.sh down
  VSVN_KEEPALIVE_TIMEOUT=1 VSVN_MAX_KEEPALIVE_REQUESTS=5 scripts/test-env/visualsvn-like.sh up
  ```

- **`scripts/test-env/flaky-tls-proxy.py`**: a TLS proxy in front of either Apache. It never sends
  `close_notify`. Faults:
  - `--close-after N`: closes a reused connection after N exchanges, under the next request.
  - `--reset-after N`: the same, with a reset.
  - `--idle-reset S`: resets a connection that stayed idle for S seconds.
  - `--stale-after S`: resets a connection idle for S seconds or more under the next request,
    without forwarding it: a proxy, firewall or NAT that dropped the idle connection silently.
  - `--log-requests`: logs the request line of every request the client starts, forwarded or
    not, so a test can check that no write was sent twice.
  - `--truncate-every K --truncate-at B`: cuts every K-th response bigger than B bytes mid-body.
  - `--truncate-method M`: counts and cuts only responses to these methods (`REPORT,GET`). With
    `--truncate-every 1` every such response is cut at the same offset: a server that always
    breaks at the same point.
  - `--outage-at-bytes B --outage-for S`: a network outage. Once B response bytes went to
    clients, every connection, open or new, goes silent for S seconds: bytes are dropped, no FIN,
    no RST (Wi-Fi gone, VPN reconnecting, laptop asleep, server unreachable). Connections open
    during the outage stay dead; new ones work again afterwards. The log says
    `network down for Ss` and `network up again`.
  - `--outage-file F`: the same outage, for as long as the file F exists.

  ```sh
  D=/tmp/opensvn-visualsvn
  python3 scripts/test-env/flaky-tls-proxy.py --listen 18452 --upstream svn.opensvn.test:18443 \
    --cert $D/certs/server.crt --key $D/certs/server.key --ca $D/certs/ca.crt \
    --reset-after 2 --idle-reset 2 &
  OPENSVN_COMPAT_LOCAL_URL=https://svn.opensvn.test:18452/svn-basic \
    cargo test -p opensvn --features real-engine,engine-conformance --test real_engine_dav \
    -- --ignored checkout_and_remote_status_at_the_same_time
  ```

`checkout_and_remote_status_at_the_same_time` runs a 1000-file checkout and repeated remote status
checks at the same time over the app's shared transport. The whole conformance suite can also run
through the proxy.

`checkout_cut_at_the_same_offset_stops_after_two_stalled_resumes` starts its own proxy (it needs
the certificates of `up` next to `OPENSVN_COMPAT_LOCAL_CA`) that cuts every `REPORT`/`GET` at
256 KiB, inside a 2 MiB file. The checkout must give up after the first drive and two resumes
that commit nothing new, and an update must finish it once the connection works again. It
asserts the number of cuts, so run it against the server directly, not through another faulty
proxy.

### Network outage during a checkout (`dav_resume`)

`apps/opensvn/src-tauri/src/app_tests/dav_resume.rs` drives the whole app (commands, operation
manager, registry, checkout journal, real engine) through the proxy's outage mode and compares
each result with `svn checkout -r R` (files, NODES rows, `svn status`), so it needs svn 1.15
(`OPENSVN_SVN_BIN`):

```sh
OPENSVN_COMPAT_LOCAL_URL=https://svn.opensvn.test:18443/svn-basic OPENSVN_SVN_BIN=/path/to/svn-1.15 \
  cargo test -p opensvn --features real-engine,engine-conformance --lib dav_resume -- --ignored
```

- a 90 s outage mid-checkout is waited out and the checkout resumes at its revision (HEAD moves
  on during the outage);
- an outage longer than `network.reconnectWaitMin` leaves an incomplete working copy listed with
  "Continue checkout"; the checkout dialog offers Continue for the same folder; once the network
  is back Continue finishes it;
- the app process is killed mid-checkout; after a restart the working copy is listed as an
  interrupted checkout, its lock (ours, dead process) is cleaned without a prompt, and Continue
  finishes it.

### App UI

`scripts/test-env/ui-checkout.mjs` drives the built app through `tauri-driver` (WebKitWebDriver,
Xvfb): the checkout dialog with the parent-path URL (the repository list message) and a
checkout of a repository with a Vietnamese name over SSO. See the script header for the build
and run commands.

## Results (2026-10-14, Ubuntu 24.04, MIT krb5 1.20.1, Apache 2.4.58, mod_auth_gssapi 1.6.4)

| Suite | mod_dav_svn 1.14.3 | 1.15.0 |
| --- | --- | --- |
| `real_engine_dav`, Basic (`/svn-basic`) | 18/18 | 18/18 |
| `real_engine_dav`, SSO, Negotiate only (`/svn-sso`) | 18/18 | 18/18 |
| `real_engine_dav`, SSO, Negotiate + Basic (`/svn`) | 18/18 | 18/18 |
| `visualsvn_like` (without `big_`) | 13/13 | 13/13 |
| `visualsvn_like` `big_*` | 2/2 | – |
| `negotiate_gssapi_kdc` (svn-ra, this KDC) | 1/1 | |
| `ui-checkout.mjs` (app UI) | 6/6 | |

Large content, release build, same container (svn 1.14.3 CLI for comparison):

| | OpenSVN | svn CLI |
| --- | --- | --- |
| commit 120 MB file | 6.9 s, peak RSS flat | |
| checkout 120 MB | 2.2 s | |
| commit of a 1-byte change in 40 MB | 42 MB uploaded (#132) | 9 KB |
| commit 10k files | 158 s | 44 s |
| checkout 10k files | 100 s (#121) | 4.5 s |

### Samba AD DC (2026-10-15, Samba 4.19.5, same Apache and mod_auth_gssapi)

| Suite | mod_dav_svn 1.14.3 | 1.15.0 |
| --- | --- | --- |
| `real_engine_dav`, Basic (`/svn-basic`) | 27/27 | 27/27 |
| `real_engine_dav`, SSO, Negotiate only (`/svn-sso`) | 27/27 | 27/27 |
| `real_engine_dav`, SSO, Negotiate + Basic (`/svn`) | 27/27 | |
| `visualsvn_like` (without `big_`) | 18/18 | 18/18 |
| `visualsvn_like` `big_*` (debug build) | 2/2 | – |

Through `flaky-tls-proxy.py --idle-reset 2 --stale-after 3`, `real_engine_dav` passes 26/27 on
both versions; the one left, `writes_after_a_pause_survive_a_proxy_that_dropped_the_idle_connection`,
starts its own proxy and passes when run against the server directly. With `--reset-after 2`
(every reused connection reset under its third request, writes included) the cases whose
commit, lock or copy is cut fail by design (a cut write is never sent again): 16 of 27, the same 15
as on main plus the new `branch_detection_follows_a_switch`, which copies a branch first.

Memory of a 1 GiB download plus a 1 GiB upload through TLS
(`crates/svn-ra/tests/transport_streaming.rs::one_gib_download_and_upload_keep_memory_flat`,
release build, 4 cores; budget 50 MiB, checked every night):

| Run | Download | Upload | Baseline RSS | Peak growth |
| --- | --- | --- | --- | --- |
| 1 | 0.71 s | 0.52 s | 9 MiB | 3 MiB |
| 2 | 0.73 s | 0.48 s | 9 MiB | 4 MiB |
| 3 | 0.82 s | 0.52 s | 9 MiB | 3 MiB |
| 4 | 0.65 s | 0.53 s | 9 MiB | 3 MiB |

```sh
cargo test --release -p svn-ra --test transport_streaming -- --ignored --exact \
  one_gib_download_and_upload_keep_memory_flat --nocapture
```

Fixed with this environment: repository listings never showed locks over DAV; locking an
`svn:needs-lock` file left it read-only; the server URL `https://host/svn/` failed with
"HTTP 500". Behaving as designed: SSO without any prompt, no-ticket and short-name/IP fallbacks
to the Basic prompt, `negotiate-failed` on Negotiate-only locations and on `http://`, hook
messages (pre-commit, pre-lock, in Vietnamese), a failing post-commit hook keeps the commit,
keywords/eol, path-based authz, case variants of a repository name (switching between two
spellings is refused, as `svn switch` refuses it).
