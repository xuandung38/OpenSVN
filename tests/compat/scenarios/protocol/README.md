# Protocol checks

`svn-protocol` is not an engine, so its compat checks do not fit the svn-vs-engine scenario DSL.
They run as Rust tests against the same servers:

```sh
cargo xtask compat up
cargo test -p svn-protocol --test compat_servers -- --ignored --nocapture --test-threads=1
```

The test replays the request builders (OPTIONS, stat, list, list-report, log, get-locks, update
send-all and skelta, file-revs) against svn 1.14.5 and 1.15.0, feeds the answers to the parsers
and asserts the editor-drive invariants. It is read-only, so the shared fixture repositories stay
untouched.

`compat_servers_write` replays the write paths (create-txn, MKCOL, PUT with svndiff, PROPPATCH, COPY,
DELETE, LOCK/UNLOCK, the `If:` header, MERGE with a lock-token list, abort) against a throwaway
repository created per run in the server container (`svnadmin create`, always removed afterwards)
and checks the result with `svnlook` (log, cat, propget, changed, lock, lstxns):

```sh
cargo test -p svn-protocol --test compat_servers_write -- --ignored --nocapture --test-threads=1
```

The recorded-traffic goldens and how they were captured are described in
`crates/svn-protocol/PROTOCOL-NOTES.md`.
