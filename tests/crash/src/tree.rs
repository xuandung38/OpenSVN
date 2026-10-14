//! Snapshots of a working copy tree on disk (administrative `.svn` directories excluded).

use std::collections::BTreeMap;
use std::path::Path;

use crate::{Result, err};

/// An entry of a [`snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A directory.
    Dir,
    /// A file with its bytes.
    File(Vec<u8>),
}

/// Every file and directory below `root` (relative `/`-separated paths, root excluded).
pub fn snapshot(root: &Path) -> Result<BTreeMap<String, Entry>> {
    let mut out = BTreeMap::new();
    walk(root, "", &mut out)?;
    Ok(out)
}

fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Entry>) -> Result<()> {
    let rd = std::fs::read_dir(dir).map_err(|e| err(format!("read_dir {}: {e}", dir.display())))?;
    for entry in rd {
        let entry = entry.map_err(|e| err(format!("read_dir {}: {e}", dir.display())))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".svn" {
            continue;
        }
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let ft = entry
            .file_type()
            .map_err(|e| err(format!("stat {rel}: {e}")))?;
        if ft.is_dir() {
            out.insert(rel.clone(), Entry::Dir);
            walk(&entry.path(), &rel, out)?;
        } else {
            let bytes =
                std::fs::read(entry.path()).map_err(|e| err(format!("read {rel}: {e}")))?;
            out.insert(rel, Entry::File(bytes));
        }
    }
    Ok(())
}

/// Copies a directory tree (`cp -a`), used to clone template working copies.
pub fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    let out = std::process::Command::new("cp")
        .arg("-a")
        .arg(from)
        .arg(to)
        .output()
        .map_err(|e| err(format!("cp: {e}")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(err(format!("cp: {}", String::from_utf8_lossy(&out.stderr))))
    }
}
