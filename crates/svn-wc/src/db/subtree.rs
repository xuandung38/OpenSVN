//! Bulk read of one subtree for walkers that visit every node (status): the NODES rows (all
//! layers), the ACTUAL_NODE rows and the repository lock cache, read in one read transaction
//! with three statements instead of several statements per node.
//!
//! The rows are decoded by the same code as the single-node readers, so a walker that looks
//! them up here sees exactly what [`WcDb::read_node`], [`WcDb::actual_props`] and friends
//! would return for the same paths. The pristine properties of NODES rows are the exception:
//! they are kept as stored ([`LazyProps`]) and decoded only for the rows a walker asks about,
//! since a status walk of a large tree needs them for few nodes.

use rusqlite::params;
use svn_types::{RelPath, Result};

use super::actual::{ConflictRecord, ConflictTexts};
use super::connection::sql_err;
use super::connection::{OpenMode, open_connection};
use super::misc::LockInfo;
use super::nodes::{
    NODE_COLUMNS, Node, Props, decode_props, node_from_row_raw, rel, strict_descendant,
};
use super::{ADM_DIR, DB_FILE, WcDb, failpoint};
use crate::skel::Skel;

/// One ACTUAL_NODE row, as far as status needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActualRow {
    /// Working copy relative path.
    pub relpath: RelPath,
    /// Locally changed properties (`None` when the row records no property change).
    pub properties: Option<Props>,
    /// Changelist membership.
    pub changelist: Option<String>,
    /// Conflict record (`None` without `conflict_data`).
    pub conflict: Option<ConflictRecord>,
}

/// One row of the repository LOCK cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockRow {
    /// `REPOSITORY.id` of the locked path.
    pub repos_id: i64,
    /// Repository relative path of the locked node.
    pub repos_relpath: String,
    /// The lock.
    pub lock: LockInfo,
}

/// The `properties` column of one NODES row, decoded when first asked for.
#[derive(Debug, Clone, Default)]
pub struct LazyProps {
    raw: Option<Box<[u8]>>,
    decoded: std::sync::OnceLock<Props>,
}

impl LazyProps {
    fn new(raw: Option<Vec<u8>>) -> Self {
        Self {
            raw: raw.map(Vec::into_boxed_slice),
            decoded: std::sync::OnceLock::new(),
        }
    }

    /// The properties, as [`Node::properties`] of the same row would hold them (`None` when
    /// the row records no property information).
    pub fn get(&self) -> Result<Option<&Props>> {
        let Some(raw) = &self.raw else {
            return Ok(None);
        };
        if let Some(p) = self.decoded.get() {
            return Ok(Some(p));
        }
        let p = decode_props(raw)?;
        Ok(Some(self.decoded.get_or_init(|| p)))
    }

    /// True when [`Self::get`] yields no property (no information, an empty blob or an empty
    /// list), decided without decoding: a non-empty list holds at least one atom.
    pub fn is_empty(&self) -> bool {
        self.raw.as_deref().is_none_or(|r| {
            r.iter()
                .all(|b| matches!(b, b'(' | b')') || b.is_ascii_whitespace())
        })
    }

    /// False when the stored list certainly has no property `name` (its name does not occur
    /// in the bytes), so a caller looking for one property decodes only the lists that may.
    pub fn may_have(&self, name: &str) -> bool {
        let needle = name.as_bytes();
        self.raw
            .as_deref()
            .is_some_and(|r| !needle.is_empty() && r.windows(needle.len()).any(|w| w == needle))
    }
}

/// Rows of the subtree of one path, see [`WcDb::read_subtree`].
#[derive(Debug, Clone, Default)]
pub struct SubtreeRows {
    /// NODES rows of the root and everything below it, ordered by path (bytewise) then
    /// `op_depth`. [`Node::properties`] is always `None` here; the properties of
    /// `nodes[i]` are `pristine[i]`.
    pub nodes: Vec<Node>,
    /// Pristine properties of each row of `nodes`, at the same index.
    pub pristine: Vec<LazyProps>,
    /// ACTUAL_NODE rows of the root and everything below it, ordered by path (bytewise).
    pub actual: Vec<ActualRow>,
    /// The whole LOCK cache (locks are matched by repository location, not by local path).
    pub locks: Vec<LockRow>,
}

impl WcDb {
    /// Every NODES and ACTUAL_NODE row at and below `root` plus the lock cache, read inside one
    /// read transaction so the three tables are seen at the same moment.
    pub fn read_subtree(&self, root: &RelPath) -> Result<SubtreeRows> {
        let wc_id = self.wc_id;
        self.with_read(|c| {
            let tx = c.unchecked_transaction().map_err(sql_err)?;
            let (nodes, pristine) = self.node_rows_split(&tx, root)?.into_iter().unzip();
            let out = SubtreeRows {
                nodes,
                pristine,
                actual: actual_rows(&tx, wc_id, root)?,
                locks: lock_rows(&tx)?,
            };
            // Read-only: ending the transaction cannot lose anything.
            tx.rollback().map_err(sql_err)?;
            Ok(out)
        })
    }
}

impl WcDb {
    /// [`node_rows`], for the whole working copy split by rowid over several read-only
    /// connections that decode their part in parallel. `tx` holds the read transaction (and
    /// with it SQLite's shared lock), so no writer can commit while the parts are read: every
    /// connection sees the same database. A part whose connection cannot be opened is read on
    /// `tx`.
    fn node_rows_split(&self, tx: &rusqlite::Connection, root: &RelPath) -> Result<Vec<Row>> {
        let wc_id = self.wc_id;
        let (lo, hi): (Option<i64>, Option<i64>) = tx
            .query_row("SELECT min(rowid), max(rowid) FROM nodes", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map_err(sql_err)?;
        let parts = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        let (Some(lo), Some(hi)) = (lo, hi) else {
            return Ok(Vec::new());
        };
        let span = hi.saturating_sub(lo).saturating_add(1);
        if !root.is_root() || parts < 2 || span < SPLIT_ROWS {
            return node_rows(tx, wc_id, root);
        }
        let parts = i64::try_from(parts).unwrap_or(1);
        let step = (span + parts - 1) / parts;
        let ranges: Vec<(i64, i64)> = (0..parts)
            .map(|i| {
                let a = lo.saturating_add(step.saturating_mul(i));
                (a, a.saturating_add(step - 1).min(hi))
            })
            .collect();
        let path = self.wcroot.join(ADM_DIR).join(DB_FILE);
        let helpers: Vec<Option<Result<Vec<Row>>>> = std::thread::scope(|scope| {
            let handles: Vec<_> = ranges[1..]
                .iter()
                .map(|&(a, b)| {
                    let path = &path;
                    scope.spawn(move || helper_rows(path, wc_id, a, b))
                })
                .collect();
            let first = rowid_rows(tx, wc_id, ranges[0].0, ranges[0].1);
            std::iter::once(Some(first))
                .chain(handles.into_iter().map(|h| h.join().ok().flatten()))
                .collect()
        });
        let mut out = Vec::new();
        for (part, &(a, b)) in helpers.into_iter().zip(&ranges) {
            out.extend(match part {
                Some(Ok(rows)) => rows,
                // `tx` already holds the shared lock, so it reads on whatever stopped the helper
                // (a real error comes back from this read too).
                Some(Err(_)) | None => rowid_rows(tx, wc_id, a, b)?,
            });
        }
        sort_nodes(&mut out);
        Ok(out)
    }
}

/// One part of [`WcDb::node_rows_split`] read on its own read-only connection; `None` when the
/// connection cannot be opened.
///
/// The helper never waits for a lock. While `tx` holds its shared lock, a writer that wants to
/// commit takes PENDING and waits for `tx` to finish; a new connection cannot take a shared
/// lock past PENDING, so waiting here would deadlock both until the busy timeout (`WcBusy`).
/// A busy helper gives its part back to `tx`, which can still read.
fn helper_rows(path: &std::path::Path, wc_id: i64, a: i64, b: i64) -> Option<Result<Vec<Row>>> {
    let conn = open_connection(path, OpenMode::ReadOnly).ok()?;
    if conn.busy_timeout(std::time::Duration::ZERO).is_err() {
        return None;
    }
    let read = || -> Result<Vec<Row>> {
        failpoint!("wc.subtree.helper_before_read");
        rowid_rows(&conn, wc_id, a, b)
    };
    Some(read())
}

/// From this many NODES rowids up the whole-tree read is split over several connections.
const SPLIT_ROWS: i64 = 4096;

/// NODES rows of the working copy with a rowid in `a..=b`.
fn rowid_rows(conn: &rusqlite::Connection, wc_id: i64, a: i64, b: i64) -> Result<Vec<Row>> {
    let sql =
        format!("SELECT {NODE_COLUMNS} FROM nodes WHERE rowid BETWEEN ?2 AND ?3 AND wc_id = ?1");
    let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
    let mut rows = stmt.query(params![wc_id, a, b]).map_err(sql_err)?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(sql_err)? {
        let (node, props) = node_from_row_raw(r)?;
        out.push((node, LazyProps::new(props)));
    }
    Ok(out)
}

/// One NODES row with its undecoded properties.
type Row = (Node, LazyProps);

/// Orders rows by path (bytewise, like SQLite's BINARY collation) then `op_depth`.
fn sort_nodes(rows: &mut [Row]) {
    fn key((n, _): &Row) -> (&[u8], u32) {
        (n.relpath.as_str().as_bytes(), n.op_depth)
    }
    // Rows usually come in path order already (the table grows in checkout order).
    if !rows.is_sorted_by(|a, b| key(a) <= key(b)) {
        rows.sort_unstable_by(|a, b| key(a).cmp(&key(b)));
    }
}

/// `WHERE` clause selecting `root` and its descendants (`?1` = wc_id, `?2` = root). The root of
/// the working copy is the whole table, read without the index.
fn subtree_filter(root: &RelPath) -> String {
    if root.is_root() {
        "wc_id = ?1 AND ?2 = ''".to_owned()
    } else {
        format!(
            "wc_id = ?1 AND (local_relpath = ?2 OR {})",
            strict_descendant("local_relpath", "?2")
        )
    }
}

/// NODES rows of the subtree, ordered by path (bytewise, like SQLite's BINARY collation) then
/// `op_depth`. Sorted here rather than with `ORDER BY`, which makes SQLite copy every row into
/// a temporary b-tree before it returns the first.
fn node_rows(conn: &rusqlite::Connection, wc_id: i64, root: &RelPath) -> Result<Vec<Row>> {
    let sql = format!(
        "SELECT {NODE_COLUMNS} FROM nodes WHERE {}",
        subtree_filter(root)
    );
    let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
    let mut rows = stmt.query(params![wc_id, root.as_str()]).map_err(sql_err)?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(sql_err)? {
        let (node, props) = node_from_row_raw(r)?;
        out.push((node, LazyProps::new(props)));
    }
    sort_nodes(&mut out);
    Ok(out)
}

type ActualCols = (
    String,
    Option<Vec<u8>>,
    Option<String>,
    Option<Vec<u8>>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn actual_rows(conn: &rusqlite::Connection, wc_id: i64, root: &RelPath) -> Result<Vec<ActualRow>> {
    let sql = format!(
        "SELECT local_relpath, properties, changelist, conflict_data, older_checksum, \
         left_checksum, right_checksum FROM actual_node WHERE {}",
        subtree_filter(root)
    );
    let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
    let rows = stmt
        .query_map(
            params![wc_id, root.as_str()],
            |r| -> rusqlite::Result<ActualCols> {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .map_err(sql_err)?;
    let mut out = Vec::new();
    for row in rows {
        let (path, props, changelist, conflict, older, left, right) = row.map_err(sql_err)?;
        out.push(ActualRow {
            relpath: rel(path)?,
            properties: props.as_deref().map(decode_props).transpose()?,
            changelist,
            conflict: conflict
                .map(|blob| -> Result<ConflictRecord> {
                    Ok(ConflictRecord {
                        skel: Skel::parse(&blob)?,
                        texts: ConflictTexts { older, left, right },
                    })
                })
                .transpose()?,
        });
    }
    out.sort_unstable_by(|a, b| a.relpath.as_str().cmp(b.relpath.as_str()));
    Ok(out)
}

fn lock_rows(conn: &rusqlite::Connection) -> Result<Vec<LockRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT repos_id, repos_relpath, lock_token, lock_owner, lock_comment, lock_date \
             FROM lock",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(LockRow {
                repos_id: r.get(0)?,
                repos_relpath: r.get(1)?,
                lock: LockInfo {
                    token: r.get(2)?,
                    owner: r.get(3)?,
                    comment: r.get(4)?,
                    date: r.get(5)?,
                },
            })
        })
        .map_err(sql_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql_err)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::db::nodes::encode_props;

    #[test]
    fn lazy_props_answer_without_decoding_and_decode_like_the_reader() {
        let none = LazyProps::new(None);
        assert!(none.is_empty() && none.get().unwrap().is_none() && !none.may_have("a"));
        let empty = LazyProps::new(Some(encode_props(&Vec::new())));
        assert!(empty.is_empty());
        assert_eq!(empty.get().unwrap(), Some(&Vec::new()));
        let props: Props = vec![
            (b"svn:eol-style".to_vec(), b"native".to_vec()),
            (b"svn:mime-type".to_vec(), b"text/x".to_vec()),
        ];
        let blob = encode_props(&props);
        let lazy = LazyProps::new(Some(blob.clone()));
        assert!(!lazy.is_empty());
        assert!(lazy.may_have("svn:mime-type") && !lazy.may_have("svn:special"));
        assert_eq!(lazy.get().unwrap(), Some(&decode_props(&blob).unwrap()));
        // Decoded once, then served from the cell.
        assert!(std::ptr::eq(lazy.get().unwrap().unwrap(), lazy.get().unwrap().unwrap()));
    }
}
