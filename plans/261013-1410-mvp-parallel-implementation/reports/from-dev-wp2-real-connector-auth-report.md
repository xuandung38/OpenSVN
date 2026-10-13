# WP2: real connector, auth broker and trust store

Branch `feat/wp2-real-connector-auth` (from `origin/main` 793c6f6, which includes WP0 #91). Plan:
`real-engine-wiring-plan.md`, decisions A, B, C and work package WP2.

## What changed

- **`engine/real/connector.rs` `HttpConnector`**
  - One shared `Arc<HttpTransport>` for the whole app. The transport's "saved password already
    tried" state is per transport, so a new transport per operation would retry a stale saved
    password on every background check and could lock an AD account.
  - Sessions are cached by repository root. Lookup takes the longest matching root prefix.
    Scheme and host compare case-insensitively; the path compares case-sensitively.
  - On a cache miss, `HttpRaSession::open` runs at the requested URL, so path-based
    authorization applies. The connector then reparents to `repos_root()` and caches that
    session. Concurrent opens are serialized per origin, and the connector checks the cache
    again after taking the lock.
  - `connect_with` handles both anchors: `Anchor::Root` returns the root session,
    `Anchor::Url` returns `root.reparent(url)`. When `interactive` is false, the session is
    made `.unattended()`, and a cache miss uses `open_unattended`.
  - An unattended cached session is replaced when an interactive request comes in.
  - `connect` is the same as `connect_with(Root, interactive)`.
  - The transport is rebuilt and the cache cleared when a network setting changes
    (`SettingsService::subscribe`: max connections, timeout, proxy, negotiate). The same
    happens on broker invalidations: sign-out and certificate removal. Changes to non-network
    settings keep the cache.
  - `HttpConnector::from_services(&EngineServices)` is ready for WP7.
- **`RealEngine`**
  - `locate` and `locate_repo_path` now authenticate at the requested URL through
    `connect_with(Anchor::Url)`, then take the root session with `connect_with(Anchor::Root)`.
    Both honor `ctx.interactive`. Before, they sent `OPTIONS` to the root, which a
    trunk-only authz rule forbids.
  - `ensure_authenticated` connects at the working copy URL (`WcInfo::url`).
- **`auth/trust.rs` `AppDbTrustStore`**
  - Pins are loaded into memory once at start-up, so `pinned()` never touches SQLite during
    the TLS handshake.
  - `pin` and `replace_pins` update the map first, then write the row if the broker has not
    already written it.
  - `forget` keeps the map in sync when a certificate is removed.
- **`auth/broker.rs` `AuthBroker`**
  - Takes an `Arc<dyn secure_store::CredentialStore>`. `NoSecretStore` and `CredentialPurge`
    are gone.
  - `os_credential_store()` returns the OS store. When none is available it returns
    `UnavailableCredentialStore`, which never saves to a file.
  - `stored_credentials` combines the `account` row with the OS store, using Basic only. The
    store is read in `spawn_blocking`.
  - On `save=true`, the broker writes the account metadata and the secret.
  - `sign_out` deletes the secrets (`account_delete_realm` now also returns the user name) and
    invalidates the cache.
  - `forget_certificate` handles the `certs_remove` command: it removes the row and the
    in-memory pin, then invalidates.
  - `trust_store()` and `invalidations()` are exposed for the connector.
  - **Rejected stored credentials:** `stored_credentials_rejected(origin)` withholds the saved
    password until the user saves new credentials for that origin. Today it is triggered by
    inference: a saved password was handed out, and the next call for that origin is a
    `Rejected` prompt. That covers interactive mode only. There is a
    `TODO(fix/wp0-followups)` to call it from the explicit `AuthPrompter` hook. That branch
    was not on origin when I finished.
  - The withholding is in memory only. After a restart, a stale saved password is offered once
    again.
- **Decision C**
  - `AppDeps.engine` is now `EngineSource::{Ready(Arc<dyn EngineFacade>), Build(Box<dyn FnOnce(EngineServices) -> Result<..>>)}`.
  - `EngineServices { auth, db, data_dir, settings }`.
  - The broker is built before the engine.
  - `AppDeps.credentials` carries the credential store.
  - `lib.rs` still uses `FakeEngine` (`EngineSource::Ready`) and now passes
    `os_credential_store()`.
- **Negotiate**
  - New app feature `negotiate-gssapi = ["svn-ra/negotiate-gssapi"]`, off by default.
    Turning it on for macOS/Linux release builds is left to the release/WP7 workflow; this
    repo has no release workflow yet.
  - New setting `network.negotiate` (bool, default true). When false, the connector builds
    the transport with `.with_negotiate(None)`.
  - IPC contract: `bindings.ts` regenerated, mock settings updated.
  - Network settings page: a checkbox labelled "Use Kerberos single sign-on…", in English and
    Vietnamese.

## Tests

`engine/real/tests/connector_tests.rs` runs against a scripted HTTPS server
(`tests/https_svn.rs`: hyper + rustls + rcgen). The server sends the HTTPv2 `OPTIONS` answer,
uses Basic auth, has an optional trunk-only authz rule, and serves the `checkout-send-all`
fixture.

- one `OPTIONS` per repository root. Sessions at `/trunk`, `/branches/b1`, root and background
  `/trunk/src` all reuse it, with 1 prompt and no new 401 challenge.
- background: no saved credentials gives `AuthRequired`; an untrusted certificate gives
  `TlsUntrusted`; neither emits `auth://prompt`. After an interactive sign-in, background work
  runs silently.
- saved credentials: after a "restart" (new broker and connector, same DB and store), both
  background and interactive work run without a prompt.
- accept-always: after a restart, even an unattended connect works without a prompt.
- sign-out and network setting changes reopen the session; an unrelated setting does not.
- authz deny `/` / allow `/trunk`: `svn_client::checkout` of trunk works, then
  `RealEngine::ensure_authenticated` on the WC, then a background `svn_client::update`. The
  root is never sent an `OPTIONS`.

`auth/tests.rs` covers:

- saved credentials across brokers
- unsaved answers store nothing
- a rejected saved password is withheld until new credentials are saved
- sign-out purges secrets and invalidates
- `forget_certificate`
- `AppDbTrustStore` persist and replace

## Gates

See the PR description for final counts. FE: `pnpm lint`, `typecheck` and `test` all pass
(199/199).

## Remaining

- Call `stored_credentials_rejected` from the `fix/wp0-followups` trait hook once it is
  merged. That also covers unattended mode.
- WP7: build `RealEngine` with `HttpConnector::from_services` through `EngineSource::Build`,
  and turn `negotiate-gssapi` on for macOS/Linux release builds.
- Saved passwords are written when the user answers the prompt, before the server has
  accepted them. A wrong saved password is caught by the rejection rule above.
