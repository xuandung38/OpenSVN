# Lock UX: lock held by another user (#127), unmodified needs-lock / locked files (#128)

Date: 2026-10-14. Branch `fix/lock-ux-127-128`, from origin/main 9425158, merged with
`test/e2e-flows` (#130, still open: it has the lock-permission fix). PR #135.

## #127: a refused lock names its owner and offers a confirmed Steal

**Backend**
- `AppError.lockConflicts?: LockConflict[]` (`path`, `owner`, `comment`, `createdAt`; never the
  token). Optional on the wire: older payloads still read, other errors send `null`.
- `EngineFacade::lock_conflicts(scope)`: locks the server reports now on the requested paths
  (`get-locks-report`, depth empty). A working copy path drops the lock whose token `wc.db`
  records (its own); a URL reports every lock. Real and fake engine.
- `lock` command: on `LockedByOther` it asks `lock_conflicts` right away and attaches the result
  (best effort: a failed read is logged by code only and the plain refusal is reported). The
  action stays `open-lock-dialog`. Nothing is stolen unless the request says `steal`.
- `AppState::start_wc_op_app` / `start_op_app`: operations that build their own `AppError`.

**Frontend**
- `WcLockDialog` stays open until the operation ends (`useOperationOutcome`), so the server's
  refusal reaches it. It shows each refusing lock ("path: locked by X, 2 hours ago", their
  comment), "Ask X to release the lock, or steal it below", the server message, and the Steal
  box. Steal needs the box *and* "I understand…"; the button is the danger variant.
- A refusal that arrives after the dialog was closed reopens it with the conflicts
  (`useOpNotifications` handles `open-lock-dialog` for lock operations of the open working copy).
- Unlock: `lock-token-invalid` / `locked-by-other` reveal the Break option the same way.

## #128: unmodified needs-lock and locked files are listed

- `svn-client` `StatusOptions::include_needs_lock` (default false, so `svn status` parity and the
  other callers are unchanged); the real engine's status turns it on. Locked and changelisted
  unmodified files were already listed. The fake engine lists the same files.
- Changes tab: a **Locks (n)** section under the list: unmodified files (`kind = file`, no status
  glyph) that need a lock or are locked, with a "Needs lock" or lock badge, **Lock to edit…** and
  **Unlock…**, plus the usual context menu. They are not part of `changedFiles`, so never ticked
  or committed. The Changes text filter applies; at most 200 rows render (`+N` beyond).
- Mock adapter: a lock refused by someone else now fails asynchronously with `lockConflicts`
  (like the real engine); sample data gains an unmodified needs-lock file whose lock only the
  server knows (`theme.ogg`, held by binh) and one locked by you (`spec.docx`).

## Tests
- Conformance (fake + real engine): `needs_lock_unmodified_file_in_status`,
  `lock_held_elsewhere_and_steal` (refusal, conflicts by path and by URL, holder sees none,
  explicit steal, then none).
- App: `a_lock_held_elsewhere_fails_with_its_owner_and_only_an_explicit_steal_takes_it`,
  `other_lock_failures_carry_no_lock_conflicts`; IPC wire shape `lock_conflicts_ride_on_the_error_only_when_known`.
- FE: owner + comment + double confirmation Steal from the Locks section; reopen after Cancel
  (no steal sent); Locks section Lock/Unlock and no commit tick; `lockFiles` unit test.

## Gates
See the PR body for the counts of the final head.

## Self-review
- **Data loss:** `lock_conflicts` is read only; the only write paths are the existing
  lock/unlock. A steal or break happens only with the explicit flag the dialog sends after two
  confirmations.
- **Path escape:** working copy paths go through `RelPath::new` (`node_location`), the same as
  lock; returned paths echo the request.
- **TLS:** sessions come from the existing connector; no change to trust handling.
- **Credentials / tokens:** lock tokens never leave the engine; the new log line carries the
  error code only.
- **svn 1.8–1.15:** `get-locks-report` is the report the remote status already sends (servers
  that ignore its depth are covered: locks outside the requested path are dropped). `wc.db` schema
  and format are unchanged.

## Not done
- The e2e flow "10.4 lock held by someone else → owner + Steal" stays `todo` in
  `apps/opensvn/e2e/flows.test.mjs`: #130 is not merged yet (task condition). The UI it needs is
  now in place (Locks → Lock to edit… → owner → Steal + confirm).
- The status still shows another user's lock only after a remote check; the dialog learns it from
  the refusal.
