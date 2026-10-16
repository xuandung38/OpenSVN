# Client scenarios: the engine and svn on the same working copies

Each scenario alternates `on: engine` steps (the real engine over HTTPS, driven by
`apps/opensvn/src-tauri/tests/compat_two_way.rs`) with `on: svn14` / `on: svn15` steps, then
compares the working copies with ones svn made itself through the same operations.

- `op-*.yaml`: one operation each (checkout, update, switch, commit, revert, add/delete/move,
  lock/unlock, cleanup, set depth, resolve). Test `client_operations_alternate_with_svn`.
- `interop-*.yaml`: svn 1.14 and 1.15 take over a working copy the engine made (status, update,
  commit, cleanup), and the engine takes over one svn made (update, commit). Test
  `dav_interop_engine_and_svn_share_working_copies`.

The test runs every scenario once against mod_dav_svn 1.14.x and once against 1.15.0 (the
`server:` key is overridden). Working copy names: `e` engine-made, `s` svn-made, `o` the svn
oracle, which goes through the same operations with svn only.

The engine steps need servers and clients on the host (the local backend); see
`tests/compat/README.md`, "Local backend".
