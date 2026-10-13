# WP3: real engine read path (status, diff_wc, status_remote)

Branch `feat/wp3-real-status-diff` (from `origin/main` 8c04160). Plan:
`plans/261013-1410-mvp-parallel-implementation/real-engine-wiring-plan.md` (sections G, WP3).

## Done
- `engine/real/wc_status.rs`
  - `status`: `svn_client::status` (infinity, no unmodified nodes, unversioned/ignored per query),
    sorted by path. `svn-client` already lists the unmodified locked, changelisted and switched
    nodes, so there is no second pass (covered by a test).
  - `diff_wc`: both sides in repository-normal form. Left = pristine text of BASE or the copy
    source (none for a plain add); `against = Some(rev)` reads the left side and its properties
    from the repository through `locate_repo_path` (missing at that revision = no left side).
    Right = working file converted with `working_repo_normal` (keywords contracted, eol
    normalized); none when deleted or missing. Text goes through `engine::text::file_diff`
    (binary sniffing, display budget / `too_complex`, labels identical to the fake engine:
    `p (base)`, `p (working)`, `p@rN`). A side above `CAT_LIMIT` (5 MB) gives `too_large` without
    sides unless `force_large`. A binary `svn:mime-type` gives `binary` + `mime` without sides.
    Property changes are listed (internal `svn:entry:`/`svn:wc:` names dropped). Directories
    return property changes only; a directory against a revision is `WcInvalidOperation`.
  - `status_remote`: session from `connect_with(url of the WC, ConnectOptions::at_url(ctx))`,
    `svn_client::status_remote` with unmodified nodes so clean out-of-date files are found, only
    nodes with a remote side returned. `latest_rev` is read after the comparison. `incoming` =
    log entries of the WC path in `(base_rev, HEAD]`, capped at `INCOMING_CAP` (1000); the path is
    computed relative to the session URL so it works with root and anchored sessions.
- `engine/real/wc_map.rs` (section G): `WcStatus -> FileStatus` via the shared IPC mapping with
  all lock tokens stripped (`lock.mine` kept, remote lock `mine` by token comparison); a node
  that only exists in the repository is shown as `normal` with its remote side, not unversioned;
  property list diff; `ResolveChoice` mapping (Mine->MineFull, Theirs->TheirsFull, Working, Base,
  `ResolveScope::ALL`) ready for WP5a (allowed dead code outside tests until then).
- #88 nits: `SessionConnector::connect_with` documents that real connectors must override it to
  honor `interactive`; `RealEngine::client_ctx` is `pub(crate)`.

## Tests (`engine/real/tests/wc_status_tests.rs`, 13 tests + 4 in `wc_map`)
Root-anchored ("flat") in-memory repository, working copies checked out with
`svn_client::checkout` over `StaticConnector` (facade checkout is WP4; `FakeRaSession::at` is
WP0). Status/diff/conflict scenarios of `engine::conformance` are mirrored and the IPC output is
compared with `FakeEngine` on the same tree: edit (status + diff equal), add/delete (equal except
the fake's size 0 for a deleted file), lock + changelist on unmodified files (equal except token
and lock date), property change (status equal), diff against a revision (sides equal), update
conflict (text status equal). Real-only: missing file, eol/keyword normal form, mime/binary/too
large/force_large, remote status (incoming, latest, remote lock without token, remote addition,
background ctx), incoming count, `connect_with` options (anchor Url, interactivity of the call),
cancellation, not-a-working-copy.

## Differences from the fake engine (noted, not changed)
- The fake exposes lock tokens in `FileStatus.lock.token`; the real engine never does.
- The real engine lists the WC root when it has something to show (e.g. a property change from
  `ignore_add`); the fake never lists the root.
- Real `status` also lists conflict marker files as unversioned (as svn does).
- Real `diff_wc` on a directory returns its property changes; the fake errors.

## Gates (local)
- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets -- -D warnings`: pass
- `cargo clippy -p opensvn --features real-engine --all-targets -- -D warnings`: pass
- `cargo test -p opensvn`: 249 passed, 0 failed
- `cargo test -p opensvn --features opensvn/real-engine`: 334 passed, 0 failed (the
  `copy_reports_what_the_repository_cannot_do` failure noted in the WP-S report no longer occurs)
- svn 1.15 oracle not used (no svn-client changes).

## Remaining
- Run `conformance::run_all` against the real engine once WP4 (checkout) and WP0
  (`FakeRaSession::at`, reparent) land (WP8a); status_remote on a non-root WC needs a session
  anchored at the WC URL (WP0/WP2), which the default `connect_with` cannot give.
