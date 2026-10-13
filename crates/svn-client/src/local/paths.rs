//! Conversion between absolute filesystem paths and working-copy relative paths.

use std::path::{Component, Path, PathBuf};

use svn_types::{ErrorCode, RelPath, Result, SvnError};
use svn_wc::WcDb;
use svn_wc::db::ADM_DIR;

/// Converts an absolute path inside the working copy into a [`RelPath`].
///
/// Fails with `NotAWorkingCopy` when the path lies outside the root and with `UnsafePath` for
/// `..` segments or the administrative directory.
pub fn rel_of(db: &WcDb, path: &Path) -> Result<RelPath> {
    let stripped = path.strip_prefix(db.wcroot()).map_err(|_| {
        SvnError::new(
            ErrorCode::NotAWorkingCopy,
            format!(
                "'{}' is not inside the working copy '{}'",
                path.display(),
                db.wcroot().display()
            ),
        )
    })?;
    let mut parts: Vec<&str> = Vec::new();
    for c in stripped.components() {
        match c {
            Component::CurDir => {}
            Component::Normal(os) => {
                let s = os.to_str().ok_or_else(|| {
                    SvnError::new(
                        ErrorCode::UnsafePath,
                        format!("path '{}' is not valid UTF-8", path.display()),
                    )
                })?;
                if s == ADM_DIR {
                    return Err(SvnError::new(
                        ErrorCode::UnsafePath,
                        format!("'{}' is inside the administrative area", path.display()),
                    ));
                }
                parts.push(s);
            }
            _ => {
                return Err(SvnError::new(
                    ErrorCode::UnsafePath,
                    format!("unsupported path '{}'", path.display()),
                ));
            }
        }
    }
    RelPath::new(&parts.join("/"))
}

/// Absolute filesystem path of `rel` inside the working copy.
pub fn abs_of(db: &WcDb, rel: &RelPath) -> PathBuf {
    let mut p = db.wcroot().to_path_buf();
    for c in rel.components() {
        p.push(c);
    }
    p
}

/// Number of components of `rel` (root is 0); equals the `op_depth` of an operation rooted there.
pub fn depth_of(rel: &RelPath) -> u32 {
    u32::try_from(rel.components().count()).unwrap_or(u32::MAX)
}
