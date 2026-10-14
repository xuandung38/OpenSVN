//! Batched commits of the update editor.
//!
//! Each node of a drive used to cost its own wc.db transaction (with `synchronous=FULL`, several
//! fsyncs), its own pristine fsync and a work queue run with two more transactions. The editor
//! now prepares each node exactly as before but hands the database part to a [`Batch`] instead
//! of committing it: a closure over [`WcTxn`] holding everything the node's transaction wrote.
//! A flush stages the batch and starts a durability barrier for it (one `syncfs` of the working
//! copy, on its own thread); the next flush waits for that barrier (normally finished by then),
//! applies the closures **in order** in ONE transaction and runs the queued work. Progress
//! events are reported as each node is prepared, like svn. The work items that ran are completed in the transaction of the following batch,
//! whose barrier covers the files they installed; [`Batch`] users drain everything at the end
//! of the drive.
//!
//! Durability ordering is the one of `svn_wc::durable`: every pristine text and every
//! installed working file is on disk before the transaction that relies on it commits.
//!
//! Crash safety is unchanged: the committed state is always a prefix of the node sequence, each
//! node all or nothing, which is exactly what a kill between two per-node transactions left
//! before. Uncommitted nodes leave their directory `incomplete` (or the old BASE), so the next
//! update fetches them again; placed pristines without a row are orphans the next install
//! overwrites. No SQLite lock is held between editor calls, so network waits never block other
//! processes.
//!
//! Reads during a drive see the committed state only. That is enough because the editor reads
//! a path the first time the drive visits it, and no earlier node of the same drive writes it,
//! with one exception: a path deleted and then added again (replacement). [`Batch::touches`]
//! reports such a path so the editor flushes first.

use std::collections::HashSet;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use svn_types::{RelPath, Result};
use svn_wc::db::WcTxn;
use svn_wc::workqueue::Unsettled;
use tempfile::TempPath;

/// The database side of one node: everything its transaction writes.
pub(super) type Op = Box<dyn FnOnce(&WcTxn<'_>) -> Result<()> + Send>;

/// Nodes per transaction before a flush.
const MAX_OPS: usize = 2048;
/// Bytes of new text placed in the pristine store before a flush.
const MAX_TEXT_BYTES: u64 = 64 << 20;
/// Age of the oldest pending node before a flush (bounds the progress a kill can lose and keeps
/// notifications flowing).
const MAX_AGE: Duration = Duration::from_secs(1);

/// Pending nodes of the update editor; see the module documentation.
#[derive(Default)]
pub(super) struct Batch {
    ops: Vec<Op>,
    /// Temporary files the queued work installs from; kept once the transaction commits.
    keep: Vec<TempPath>,
    /// Paths removed by pending nodes.
    removed: HashSet<RelPath>,
    since: Option<Instant>,
    text_bytes: u64,
    /// Pristine texts were placed without their own sync.
    unsynced: bool,
}

impl Batch {
    /// Adds a node; `true` when the batch is due for a flush.
    pub(super) fn push(&mut self, op: Op) -> bool {
        self.ops.push(op);
        self.since.get_or_insert_with(Instant::now);
        self.is_full()
    }

    /// Notes `path` as removed by the pending nodes (see [`touches`](Self::touches)).
    pub(super) fn removed(&mut self, path: RelPath) {
        self.removed.insert(path);
    }

    /// True when a pending node removed `path`, so it must be committed (and its work run)
    /// before the drive looks at `path` again.
    pub(super) fn touches(&self, path: &RelPath) -> bool {
        self.removed.contains(path)
    }

    /// Records a new text placed in the pristine store for a pending node.
    pub(super) fn placed_text(&mut self, bytes: u64, synced: bool) {
        self.text_bytes += bytes;
        self.unsynced |= !synced;
    }

    /// Keeps `tmp` (a merge result the queued work installs from) once the batch commits.
    pub(super) fn keep_on_commit(&mut self, tmp: TempPath) {
        self.keep.push(tmp);
    }

    fn is_full(&self) -> bool {
        self.ops.len() >= MAX_OPS
            || self.text_bytes >= MAX_TEXT_BYTES
            || self.since.is_some_and(|t| t.elapsed() >= MAX_AGE)
    }

    /// True when nothing is pending.
    pub(super) fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Takes the pending content, leaving the batch empty.
    pub(super) fn take(&mut self) -> Pending {
        let taken = std::mem::take(self);
        Pending {
            deleted: !taken.removed.is_empty(),
            ops: taken.ops,
            keep: taken.keep,
            unsynced: taken.unsynced,
        }
    }
}

/// What a flush commits.
pub(super) struct Pending {
    pub(super) ops: Vec<Op>,
    pub(super) keep: Vec<TempPath>,
    /// Pristine texts of the nodes were placed without their own sync.
    pub(super) unsynced: bool,
    /// A node removes something (crash suite failpoint `update.after_delete_txn`).
    pub(super) deleted: bool,
}

/// A batch waiting for its durability barrier, which runs on another thread while the editor
/// prepares the next batch. Committed by the next flush (or the final drain).
pub(super) struct Staged {
    pub(super) pending: Pending,
    /// Work items that ran after the previous commit; completed with this batch.
    pub(super) settle: Unsettled,
    /// `syncfs` of the working copy, started after every file of `pending` and `settle` was
    /// written; `None` when nothing needs it (each file was synced on its own).
    pub(super) ticket: Option<JoinHandle<std::io::Result<()>>>,
}
