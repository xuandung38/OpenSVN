//! The `file-commit` work item (`process_commit_file_install` in svn's `workqueue.c`).
//!
//! Commit post-processing folds the new revision into the database and queues one `file-commit`
//! item per committed file in the same transaction (see [`super::finalize`]). The item then makes
//! the working file match the new BASE: keywords such as `$Rev$` are re-expanded with the new
//! revision, line endings follow `svn:eol-style`, the read-only and executable bits follow
//! `svn:needs-lock` and `svn:executable`, and the stat cache is refreshed when the file equals the
//! committed text.
//!
//! The handler is installed into every work queue run of this crate
//! ([`crate::wc::run_work_queue`]), so an item left behind by a crash is finished by the next
//! operation, by `cleanup`, or by svn itself (the item has svn's on-disk format).
//!
//! Idempotent, as the work queue requires: running it twice leaves the same file and rows.

use std::borrow::Cow;
use std::fs;

use svn_types::{ErrorCode, NodeKind, RelPath, Result, SvnError, write_file_replacing};
use svn_wc::WcDb;
use svn_wc::db::{DbWorkingCopyOps, Presence};
use svn_wc::pristine::Sha1Digest;
use svn_wc::translate::{self, utc_offset_local};
use svn_wc::workqueue::WorkingCopyOps;

use crate::local::content::{detranslate_digest, stat_cache, translate_info};
use crate::local::paths::abs_of;

/// Post-commit fixup of the committed file at `path`; the handler passed to
/// [`DbWorkingCopyOps::with_file_commit`].
///
/// A path that is no longer a versioned file, or whose working file is gone, needs nothing.
pub fn file_commit_fixup(db: &WcDb, path: &RelPath) -> Result<()> {
    let Some(node) = db.read_node(path)? else {
        return Ok(());
    };
    if node.kind != NodeKind::File || node.presence != Presence::Normal {
        return Ok(());
    }
    let abs = abs_of(db, path);
    let Ok(md) = fs::symlink_metadata(&abs) else {
        return Ok(());
    };
    let ops = DbWorkingCopyOps::new(db);
    let tinfo = translate_info(db, path)?;

    // svn translates the working file "from normal form" again: keywords take the values of
    // the new revision and line endings the configured style. A symlink (`svn:special`) is
    // never translated.
    if md.is_file() && !tinfo.special && tinfo.translation_required() {
        let info = ops.install_info(path.as_str()).map_err(SvnError::from)?;
        let current = fs::read(&abs)
            .map_err(|e| SvnError::from_io(format!("read '{}'", abs.display()), &e))?;
        let translated =
            translate::to_working(&tinfo, &current, &info.keyword_context(), &utc_offset_local)?;
        if let Cow::Owned(bytes) = translated
            && bytes != current
        {
            write_file_replacing(&abs, &bytes)?;
        }
    }

    if md.is_file()
        && let Some(input) = ops
            .sync_flags_input(path.as_str())
            .map_err(SvnError::from)?
    {
        translate::flags::sync_with_props(&abs, input)?;
    }

    // Record the stat cache only when the file is exactly what was committed: a file the user
    // changed while the commit ran must keep showing as modified.
    let expected = node
        .checksum
        .as_deref()
        .and_then(Sha1Digest::from_svn_string);
    let Some(expected) = expected else {
        return Ok(());
    };
    let digest = match detranslate_digest(db, path, &abs) {
        Ok(d) => d,
        // Inconsistent or unknown line endings (SVN_ERR_IO_INCONSISTENT_EOL / UNKNOWN_EOL): the
        // file is simply modified. A file deleted meanwhile has no stat to cache.
        Err(e)
            if matches!(e.svn_code, Some(135_000 | 135_001))
                || e.code == ErrorCode::PathNotFound =>
        {
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    if digest.sha1 != expected {
        return Ok(());
    }
    if let Some((size, mtime)) = stat_cache(&abs) {
        db.with_txn(|t| t.record_fileinfo(path, Some(size), Some(mtime)))?;
    }
    Ok(())
}
