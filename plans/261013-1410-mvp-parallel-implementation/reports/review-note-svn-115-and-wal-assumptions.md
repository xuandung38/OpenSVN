---
name: svn-115-and-wal-assumptions
description: OpenSVN docs assume svn 1.15 released and WAL on wc.db; both unverified/contradicted as of 2026-10-13 red-team review
metadata:
  type: project
---

As of 2026-10-13, ADR-004 claims Subversion 1.15 is released, but Apache's 1.15 release-notes page still said "not released yet" (search results). ADR-003 + doc 02 assume WAL for `.svn/wc.db` while phase-07 requires svn-identical journal mode.

**Why:** Both underpin compat oracle (Docker fixtures, format 32) and WcActor concurrent-read design.

**How to apply:** In future reviews of OpenSVN plans/code, check whether these were resolved (svn_sqlite.c journal mode, 1.15 release/VisualSVN version) before accepting format-32 or concurrent wc.db read claims.
