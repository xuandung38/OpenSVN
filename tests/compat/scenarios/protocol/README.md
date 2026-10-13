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
untouched. The recorded-traffic goldens and how they were captured are described in
`crates/svn-protocol/PROTOCOL-NOTES.md`.
