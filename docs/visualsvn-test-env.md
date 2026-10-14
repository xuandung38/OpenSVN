# VisualSVN-like test environment (Linux)

A local stand-in for an internal VisualSVN Server (Apache + mod_dav_svn on Windows, Active
Directory sign-in through Integrated Windows Authentication or Basic over HTTPS), to find
client bugs before trying OpenSVN against the real server. One script builds it:
`scripts/test-env/visualsvn-like.sh`.

## What it emulates

| VisualSVN Server | Here |
| --- | --- |
| Active Directory (Kerberos KDC) | MIT krb5 KDC, realm `OPENSVN.TEST`, users `alice` / `alice-pass`, `bob` / `bob-pass` |
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

### App UI

`scripts/test-env/ui-checkout.mjs` drives the built app through `tauri-driver` (WebKitWebDriver,
Xvfb): the checkout dialog with the parent-path URL (the repository list message) and a
checkout of a repository with a Vietnamese name over SSO. See the script header for the build
and run commands.

## Results (2026-10-14, Ubuntu 24.04, MIT krb5 1.20.1, Apache 2.4.58, mod_auth_gssapi 1.6.4)

See the pull request that added this environment for the run log; summary:

| Suite | 1.14.3 | 1.15.0 |
| --- | --- | --- |
| `real_engine_dav`, Basic (`/svn-basic`) | 18/18 | – |
| `real_engine_dav`, SSO, Negotiate only (`/svn-sso`) | 18/18 | 18/18 |
| `real_engine_dav`, SSO, Negotiate + Basic (`/svn`) | 18/18 | 18/18 |
| `visualsvn_like` (without `big_`) | 10/10 | 10/10 |

Bugs found and fixed: see the pull request.
