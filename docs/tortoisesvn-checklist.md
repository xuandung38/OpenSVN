# TortoiseSVN 1.14 two-way checklist (Windows)

Manual verification that a working copy made by OpenSVN works in TortoiseSVN 1.14, and that a
working copy made by TortoiseSVN works in OpenSVN (issues #15, #16).

## What the automated tests already cover

TortoiseSVN is built on libsvn, the same library as the `svn` command line client. The two-way
compat tests run svn 1.14.3 and svn 1.15.0 on working copies the engine made, and the engine on
working copies svn made, over HTTPS against mod_dav_svn 1.14 and 1.15, and compare `wc.db`,
`svn status`, `svn info` and the files with working copies svn made itself:

- `apps/opensvn/src-tauri/tests/compat_two_way.rs` with `tests/compat/scenarios/client/`
  (checkout, update, switch, commit, revert, add/delete/move, lock/unlock, cleanup, set depth,
  resolve, inherited properties, and the `interop-*` hand-overs in both directions).
- How to run them: `tests/compat/README.md`, "Local backend".

So what is left for a person is what only TortoiseSVN on Windows does: the Explorer overlays,
the TSVNCache process that watches and reads working copies in the background, Windows file
locking and the TortoiseSVN dialogs. That is what this checklist checks.

## Setup

| Item | Value |
|---|---|
| Windows | 10 22H2 or 11, NTFS, 64-bit |
| TortoiseSVN | 1.14.x 64-bit (record the exact build: About dialog) |
| OpenSVN | the build under test (record version and commit) |
| Server | the internal VisualSVN Server, or the Linux test server of `docs/visualsvn-test-env.md` reachable over HTTPS |
| Repository | a scratch repository loaded with the `basic` fixture (`fixtures/repos/make-basic.sh`, then `svnadmin dump` / `svnadmin load` on the server) |

1. Trust the server certificate in both clients (Windows certificate store for OpenSVN, the
   TortoiseSVN prompt "Accept permanently").
2. Sign in once in each client with the same account.
3. In TortoiseSVN Settings > Icon Overlays, keep the defaults (status cache "Default", all drives
   included). Note the setting in the results.
4. On the repository root set `svn:global-ignores` to `*.bak` and commit (TortoiseSVN Properties
   dialog on a checkout of the repository root). The checklist uses it to check inherited
   properties.
5. Close every Explorer window on the test folders before each section; open them only when a
   step says so.

Working copy format: OpenSVN creates format 31 (svn 1.8 to 1.15 read it). TortoiseSVN 1.14 cannot
open a format 32 working copy (svn 1.15 `--compatible-version 1.15`); it must say "working copy
too new" (`E155021`) and change nothing. Check this once with a format 32 working copy (step A13).

## A. OpenSVN makes the working copy, TortoiseSVN takes over

Check out `<repo>/trunk` with OpenSVN into `C:\svn-test\from-opensvn`. Then:

| # | Step | Expected |
|---|---|---|
| A1 | Open the folder in Explorer. | Green "normal" overlay on the folder and on every file. No TortoiseSVN error balloon. `locked.txt` and `many-props.txt` (`svn:needs-lock`) are read-only and show the needs-lock overlay. |
| A2 | TortoiseSVN > Check for modifications. | Nothing modified. The externals `with-externals/docs-ext` and `with-externals/readme-ext.txt` are listed as externals, not as unversioned. |
| A3 | In OpenSVN: edit `README.txt`, add `new.txt`, delete `crlf.txt`, move `src/app.c` to `src/renamed.c`, set property `custom:p=v` on `docs/index.md`. Create `old.bak`. Then refresh Explorer (F5). | Overlays: modified on `README.txt` and `docs/index.md`, added on `new.txt` and `src/renamed.c`, deleted on the parent of `crlf.txt`. `old.bak` shows no overlay and is not listed as unversioned in Check for modifications (ignored through the repository's `svn:global-ignores`, read from the inherited-properties cache OpenSVN wrote). |
| A4 | TortoiseSVN > Check for modifications. | The same five changes, `src/renamed.c` shown as "moved from src/app.c". Diff of `README.txt` shows only the edit. |
| A5 | TortoiseSVN > Update. | "Completed", nothing changed. Overlays unchanged. |
| A6 | TortoiseSVN > Commit, all five changes. | Commit succeeds with one new revision; afterwards every overlay is green. TortoiseSVN Log shows the move with its copy source. |
| A7 | In OpenSVN: Update. | Succeeds, nothing to do. OpenSVN status is clean. |
| A8 | TortoiseSVN > Get lock on `locked.txt`, edit it, commit. Then in OpenSVN: status. | OpenSVN shows the file locked by this working copy before the commit, and neither locked nor modified after it. The file is read-only again. |
| A9 | In OpenSVN: lock `README.txt`. Then TortoiseSVN > Check for modifications with "Check repository". Then TortoiseSVN > Release lock. Then OpenSVN status. | TortoiseSVN shows the lock (K) with OpenSVN's comment; release succeeds; OpenSVN shows no lock. |
| A10 | Interrupt an OpenSVN update (cancel, or end the process in Task Manager during a large update). Then TortoiseSVN > Clean up with default options. | TortoiseSVN cleans up without error; OpenSVN then updates to the end. Run once more with "Break write locks" checked in TortoiseSVN Clean up while OpenSVN is closed. |
| A11 | TortoiseSVN > Clean up with "Delete unversioned files and folders", "Delete ignored files and folders" and "Vacuum pristine copies". | `old.bak` is deleted; status clean in both clients. |
| A12 | TortoiseSVN > Switch to `<repo>/branches/feature-x`, then OpenSVN Switch back to trunk. | Both succeed; `many-props.txt` is deleted by the first switch and comes back with the second; OpenSVN and TortoiseSVN agree on the URL and revision. |
| A13 | (Format 32) Make a format 32 working copy with svn 1.15 (`svn checkout --compatible-version 1.15 <url> C:\svn-test\f32`) and open it in Explorer, then TortoiseSVN > Update. | TortoiseSVN 1.14 refuses with "too new" (E155021); the working copy is unchanged and still opens in OpenSVN. |

## B. TortoiseSVN makes the working copy, OpenSVN takes over

TortoiseSVN > Checkout `<repo>/trunk` into `C:\svn-test\from-tortoise`. In a second checkout
(`C:\svn-test\other`) commit an edit of `README.txt` and a new file `incoming.txt`, so the first one
has something to update. Then in `from-tortoise`, with TortoiseSVN: edit `docs/index.md`, add
`mine.txt`, rename `data/blob.bin` to `data/moved.bin` (TortoiseSVN > Rename), delete
`mixed-eol.txt`.

| # | Step | Expected |
|---|---|---|
| B1 | Open the working copy in OpenSVN. | Opens without "needs cleanup" or a lock warning; status shows the four changes, `data/moved.bin` as moved. |
| B2 | OpenSVN: Update. | `README.txt` and `incoming.txt` arrive, the local changes stay. Explorer overlays (F5) match OpenSVN's status. |
| B3 | TortoiseSVN > Check for modifications. | Same changes as OpenSVN shows; the move is still a move (not a delete plus an add). TortoiseSVN > Show log on `data/moved.bin` works. |
| B4 | OpenSVN: Commit all changes. | One new revision; afterwards TortoiseSVN overlays are green after F5, Check for modifications is empty. |
| B5 | TortoiseSVN > Update, then Clean up. | Both succeed and change nothing. |
| B6 | Make a conflict: edit line 1 of `docs/index.md` in `other` and commit; edit line 1 in `from-tortoise`. OpenSVN: Update. | OpenSVN reports a text conflict. Explorer shows the conflict overlay; TortoiseSVN > Edit conflicts opens TortoiseMerge with the three versions. |
| B7 | Resolve in TortoiseSVN ("Resolved"), then commit from OpenSVN. | OpenSVN sees no conflict any more and commits. |
| B8 | TortoiseSVN > Get lock on `README.txt`. OpenSVN: commit an edit of it, keeping the lock unchecked (default). | The commit uses TortoiseSVN's lock token and releases it; TortoiseSVN shows no lock afterwards. |
| B9 | TortoiseSVN > Update to revision (an older one) with "Working copy depth: Immediate children" on `src`. Then OpenSVN: Update, and change the depth of `src` back to fully recursive. | OpenSVN keeps the sparse depth on Update and fetches the folder when the depth is changed; TortoiseSVN overlays agree afterwards. |

## C. TSVNCache and Windows file locking

TSVNCache.exe (the TortoiseSVN status cache) watches working copies, reads `.svn\wc.db` and may
hold files open for a moment. OpenSVN must not fail or corrupt anything because of it.

| # | Step | Expected |
|---|---|---|
| C1 | Keep an Explorer window open on `from-opensvn` (overlays visible). In OpenSVN, update across 50+ changed files, then commit 50+ files. | Both succeed. No "database is locked" error; if OpenSVN shows a "working copy busy" notice, it clears by itself (automatic retry). |
| C2 | Same with TortoiseSVN Settings > Icon Overlays > Status cache "Shell" and then "None". | Same result. Record any difference. |
| C3 | Open `README.txt` in an editor that keeps the file open (e.g. Word for `.docx`, or Notepad++ with the file in a tab), then OpenSVN: Update that changes it, and Revert. | OpenSVN reports that the file is in use (no partial file, no half-applied update) or succeeds; after closing the editor an Update or Cleanup finishes the job. `svn status` / TortoiseSVN show no damage. |
| C4 | Run a TortoiseSVN Update and an OpenSVN Update on the same working copy at the same time. | One waits or reports that the working copy is locked by another client; neither corrupts it; afterwards TortoiseSVN > Clean up is not needed (or succeeds when run). |
| C5 | Kill TSVNCache.exe in Task Manager while OpenSVN commits. | The commit is unaffected; TSVNCache restarts on its own and overlays come back. |
| C6 | Check Task Manager (Details, "Handle" via Resource Monitor) for `wc.db` handles after OpenSVN finished an operation and TSVNCache was idle for a minute. | OpenSVN holds no handle on `wc.db` or files below the working copy when idle (a held handle would block TortoiseSVN's own cleanup). |

## Results

Copy this table into the issue (#15 or #16) with the date and the versions:

| Section | Pass | Fail | Notes (TortoiseSVN build, OpenSVN commit, server) |
|---|---|---|---|
| A1-A13 | | | |
| B1-B9 | | | |
| C1-C6 | | | |

A failure in A or B that the automated scenarios did not catch is a gap in them: add a scenario
in `tests/compat/scenarios/client/` that reproduces it with svn 1.14 before fixing the engine.
