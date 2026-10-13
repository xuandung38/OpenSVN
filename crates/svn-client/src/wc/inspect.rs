//! Local change detection: is a working file or its properties different from the pristine.

use std::fs::{self, File};
use std::io::Write;
use std::time::UNIX_EPOCH;

use sha1::{Digest, Sha1};
use svn_types::{ErrorCode, NodeKind, RelPath, Result, SvnError};
use svn_wc::db::{Node, Presence, Props, WcDb};
use svn_wc::pristine::Sha1Digest;
use svn_wc::translate::{ContentTranslator, TranslateInfo, special, translate_stream};

use super::abs_path;

/// Modification time as APR microseconds.
pub fn mtime_micros(md: &fs::Metadata) -> i64 {
    match md.modified().map(|t| t.duration_since(UNIX_EPOCH)) {
        Ok(Ok(d)) => i64::try_from(d.as_micros()).unwrap_or(i64::MAX),
        Ok(Err(e)) => -i64::try_from(e.duration().as_micros()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// Property lookup helper over a stored property list.
pub fn prop_value<'a>(props: &'a Props, name: &str) -> Option<&'a [u8]> {
    props
        .iter()
        .find(|(k, _)| k == name.as_bytes())
        .map(|(_, v)| v.as_slice())
}

/// Translation-relevant properties of a property list.
pub fn translate_info(props: &Props) -> TranslateInfo {
    TranslateInfo::from_props(|n| prop_value(props, n))
}

fn sorted(p: &Props) -> Props {
    let mut v = p.clone();
    v.sort();
    v
}

/// True when the working properties of `rel` differ from the pristine ones.
pub fn props_modified(db: &WcDb, rel: &RelPath) -> Result<bool> {
    Ok(sorted(&db.actual_props(rel)?) != sorted(&db.pristine_props(rel)?))
}

struct HashWriter(Sha1);

impl Write for HashWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// SHA-1 of the working file converted to repository-normal form (what a commit would send).
fn normalized_sha1(path: &std::path::Path, info: &TranslateInfo) -> Result<[u8; 20]> {
    let meta = fs::symlink_metadata(path)
        .map_err(|e| SvnError::from_io(format!("cannot stat '{}'", path.display()), &e))?;
    let mut hasher = HashWriter(Sha1::new());
    if meta.file_type().is_symlink() || info.special {
        let text = special::read_repo_normal(path)?;
        hasher.0.update(&text);
    } else {
        let mut f = File::open(path)
            .map_err(|e| SvnError::from_io(format!("cannot open '{}'", path.display()), &e))?;
        translate_stream(&mut f, &mut hasher, ContentTranslator::to_repo_normal(info, true)?)?;
    }
    Ok(hasher.0.finalize().into())
}

/// True when the working file of the BASE-backed `node` differs from its pristine text. A missing
/// file or a kind mismatch is not a text modification (status reports those separately).
pub fn text_modified(db: &WcDb, rel: &RelPath, node: &Node) -> Result<bool> {
    let path = abs_path(db, rel)?;
    let Ok(md) = fs::symlink_metadata(&path) else {
        return Ok(false);
    };
    if !md.file_type().is_file() && !md.file_type().is_symlink() {
        return Ok(false);
    }
    if md.file_type().is_file()
        && node.translated_size == i64::try_from(md.len()).ok()
        && node.last_mod_time == Some(mtime_micros(&md))
    {
        return Ok(false);
    }
    let Some(expected) = node.checksum.as_deref().and_then(Sha1Digest::from_svn_string) else {
        return Ok(true);
    };
    let info = translate_info(&db.actual_props(rel)?);
    Ok(normalized_sha1(&path, &info)? != expected.0)
}

/// The working file converted to repository-normal form, read into memory.
pub fn working_repo_normal(db: &WcDb, rel: &RelPath) -> Result<Vec<u8>> {
    let path = abs_path(db, rel)?;
    let info = translate_info(&db.actual_props(rel)?);
    let md = fs::symlink_metadata(&path)
        .map_err(|e| SvnError::from_io(format!("cannot stat '{}'", path.display()), &e))?;
    if md.file_type().is_symlink() || info.special {
        return Ok(special::read_repo_normal(&path)?);
    }
    let mut f = File::open(&path)
        .map_err(|e| SvnError::from_io(format!("cannot open '{}'", path.display()), &e))?;
    let mut out = Vec::new();
    translate_stream(&mut f, &mut out, ContentTranslator::to_repo_normal(&info, true)?)?;
    Ok(out)
}

/// Whether `rel` or anything below it carries a local change that an incoming delete would lose:
/// edited files, changed properties, or working layers (add, delete, replace, copy, move).
pub fn subtree_has_local_changes(db: &WcDb, rel: &RelPath) -> Result<bool> {
    let Some(cur) = db.read_node(rel)? else {
        return Ok(false);
    };
    if cur.op_depth > 0 && cur.presence != Presence::BaseDeleted {
        return Ok(true);
    }
    if props_modified(db, rel)? {
        return Ok(true);
    }
    if cur.kind == NodeKind::File {
        if cur.presence == Presence::Normal && text_modified(db, rel, &cur)? {
            return Ok(true);
        }
        return Ok(false);
    }
    for child in db.children(rel)? {
        if subtree_has_local_changes(db, &child)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Error for a node that is expected to exist in BASE.
pub fn missing_base(rel: &RelPath) -> SvnError {
    SvnError::new(
        ErrorCode::PathNotFound,
        format!("'{rel}' is not in the working copy base"),
    )
}
