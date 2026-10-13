//! Creation of conflict marker files and the work items that fill them.
//!
//! Naming follows svn 1.15.0 exactly:
//!
//! * text conflict of an `update`/`switch`: `<file>.mine`, `<file>.r<OLD>`, `<file>.r<NEW>`
//!   (`libsvn_wc/update_editor.c`); of a `merge`: `<file>.working`, `<file>.merge-left.r<N>`,
//!   `<file>.merge-right.r<N>` (`libsvn_client/merge.c`). When the file's extension matches one
//!   of the configured `preserved-conflict-file-exts` patterns, it is appended again so editors
//!   still recognise the type: `<file>.r2.txt`.
//! * property conflict: `<file>.prej`, or `dir_conflicts.prej` inside a directory.
//! * every marker name is made unique like `svn_io_open_uniquely_named`: the plain name is tried
//!   first, then `<name>.2<suffix>`, `<name>.3<suffix>`, ... up to 99999, each created
//!   exclusively (so an existing file or a planted symlink is never reused or followed).
//!
//! Marker files are created empty here and filled by the returned [`WorkItem`]s
//! (`file-translate` for text markers, `prej-install` for property markers), so the whole thing
//! is crash-safe: the conflict skel and the work items are committed together and the queue
//! completes the files later.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use svn_types::{ErrorCode, RelPath, SvnError, safe_join};

use crate::translate::{ContentTranslator, TranslateInfo, special, translate_stream};
use crate::workqueue::WorkItem;

/// Name of the property reject file inside a directory (`SVN_WC__THIS_DIR_PREJ`).
pub const THIS_DIR_PREJ: &str = "dir_conflicts";
/// Extension of property reject files (`SVN_WC__PROP_REJ_EXT`).
pub const PROP_REJ_EXT: &str = ".prej";

const MAX_UNIQUE_TRIES: u32 = 99_999;

/// `svn_path_splitext`: the extension of the last path component, or `""` when there is none, it
/// is empty (`file.`), or the dot starts the name (`.profile`).
pub fn path_extension(path: &str) -> &str {
    let Some(dot) = path.rfind('.') else {
        return "";
    };
    if dot + 1 == path.len() {
        return "";
    }
    let name_start = path.rfind('/').map_or(0, |s| s + 1);
    if dot > name_start {
        &path[dot + 1..]
    } else {
        ""
    }
}

/// Parses the inside of a `[...]` set (after the `[`): returns whether it is negated, the
/// inclusive byte ranges, and the pattern after the closing `]`. A backslash escapes the next
/// byte; the first `]` right after the opening (or after `!`/`^`) is a literal.
/// Negation flag, inclusive byte ranges, and the pattern after the closing `]`.
type Bracket<'a> = (bool, Vec<(u8, u8)>, &'a [u8]);

fn parse_bracket(rest: &[u8]) -> Option<Bracket<'_>> {
    let (negate, mut i) = match rest.first() {
        Some(b'!' | b'^') => (true, 1),
        _ => (false, 0),
    };
    let mut items = Vec::new();
    let mut first = true;
    while i < rest.len() {
        let mut c = rest[i];
        if c == b']' && !first {
            return Some((negate, items, &rest[i + 1..]));
        }
        first = false;
        if c == b'\\' && i + 1 < rest.len() {
            i += 1;
            c = rest[i];
        }
        i += 1;
        if i + 1 < rest.len() && rest[i] == b'-' && rest[i + 1] != b']' {
            let mut hi = rest[i + 1];
            i += 2;
            if hi == b'\\' && i < rest.len() {
                hi = rest[i];
                i += 1;
            }
            items.push((c, hi));
        } else {
            items.push((c, c));
        }
    }
    None
}

/// `svn_cstring_match_glob_list` (`apr_fnmatch` with flags 0: `*`, `?`, `[...]` with ranges and
/// negation, backslash escapes, case sensitive).
pub(crate) fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|i| glob_match(rest, &text[i..])),
        Some((b'?', rest)) => text.split_first().is_some_and(|(_, t)| glob_match(rest, t)),
        // `\x` matches the literal `x` (apr_fnmatch without APR_FNM_NOESCAPE). A trailing lone
        // backslash matches itself.
        Some((b'\\', rest)) => match rest.split_first() {
            Some((&escaped, after)) => {
                text.first() == Some(&escaped) && glob_match(after, &text[1..])
            }
            None => text == b"\\",
        },
        Some((b'[', rest)) => {
            let Some((negate, items, after)) = parse_bracket(rest) else {
                // No closing ']': '[' is an ordinary character.
                return text.first() == Some(&b'[') && glob_match(rest, &text[1..]);
            };
            let Some((&c, t)) = text.split_first() else {
                return false;
            };
            let hit = items.iter().any(|&(lo, hi)| (lo..=hi).contains(&c));
            hit != negate && glob_match(after, t)
        }
        Some((&p, rest)) => text.first() == Some(&p) && glob_match(rest, &text[1..]),
    }
}

/// Extension to keep in marker names: the file's extension when it matches one of `patterns`
/// (the `preserved-conflict-file-exts` setting), `""` otherwise or when `patterns` is empty.
pub fn preserved_extension<'a>(local_relpath: &'a str, patterns: &[String]) -> &'a str {
    let ext = path_extension(local_relpath);
    if !ext.is_empty()
        && patterns
            .iter()
            .any(|p| glob_match(p.as_bytes(), ext.as_bytes()))
    {
        ext
    } else {
        ""
    }
}

fn with_ext(label: String, ext: &str) -> String {
    if ext.is_empty() {
        label
    } else {
        format!("{label}.{ext}")
    }
}

/// Suffixes appended to the file name of the three text conflict markers.
///
/// Intentional difference from localized svn: `libsvn_client/merge.c` runs the merge labels
/// through gettext (`ja.po` even drops the preserved extension), so real svn output varies per
/// machine. OpenSVN always uses the C-locale names below, like the `$Date$` text in `translate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMarkerLabels {
    /// Common ancestor (`.r<OLD>` / `.merge-left.r<N>`).
    pub original: String,
    /// The user's version (`.mine` / `.working`).
    pub mine: String,
    /// The incoming version (`.r<NEW>` / `.merge-right.r<N>`).
    pub their: String,
}

impl TextMarkerLabels {
    /// Labels for an `update` or `switch`. An invalid old revision (conflict against a local
    /// addition) is `0`, as in svn.
    pub fn for_update(old_revision: Option<i64>, new_revision: i64, ext: &str) -> Self {
        let old = old_revision.filter(|r| *r >= 0).unwrap_or(0);
        Self {
            original: with_ext(format!(".r{old}"), ext),
            mine: with_ext(".mine".into(), ext),
            their: with_ext(format!(".r{new_revision}"), ext),
        }
    }

    /// Labels for a `merge` of `left_revision` to `right_revision`.
    pub fn for_merge(left_revision: i64, right_revision: i64, ext: &str) -> Self {
        Self {
            original: with_ext(format!(".merge-left.r{left_revision}"), ext),
            mine: with_ext(".working".into(), ext),
            their: with_ext(format!(".merge-right.r{right_revision}"), ext),
        }
    }
}

fn io_err(what: &str, path: &Path, e: &std::io::Error) -> SvnError {
    SvnError::from_io(format!("{what} {}", path.display()), e)
}

/// `svn_io_open_uniquely_named` without keeping the file open: creates `<dir>/<name><suffix>`,
/// or `<dir>/<name>.<n><suffix>` for n = 2.., exclusively, and returns the path created.
pub fn create_unique_file(dir: &Path, name: &str, suffix: &str) -> Result<PathBuf, SvnError> {
    for i in 1..=MAX_UNIQUE_TRIES {
        let file_name = if i == 1 {
            format!("{name}{suffix}")
        } else {
            format!("{name}.{i}{suffix}")
        };
        let path = dir.join(file_name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            // Windows reports a colliding directory name as access denied.
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && path.exists() => {}
            Err(e) => return Err(io_err("create", &path, &e)),
        }
    }
    Err(SvnError::new(
        ErrorCode::Internal,
        format!(
            "unable to make a unique marker name for {} in {}",
            name,
            dir.display()
        ),
    ))
}

/// Splits a relpath into parent relpath and file name.
fn split(relpath: &str) -> (&str, &str) {
    relpath.rsplit_once('/').unwrap_or(("", relpath))
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

fn unsafe_rel(relpath: &str, e: &SvnError) -> SvnError {
    SvnError::new(ErrorCode::UnsafePath, format!("{relpath}: {}", e.message))
}

fn abs_dir(wc_root: &Path, relpath: &str) -> Result<PathBuf, SvnError> {
    let rel = RelPath::new(relpath).map_err(|e| unsafe_rel(relpath, &e))?;
    let dir = safe_join(wc_root, &rel).map_err(|e| unsafe_rel(relpath, &e))?;
    // `safe_join` leaves the last component unchecked; markers are created *inside* it.
    if fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(SvnError::new(
            ErrorCode::UnsafePath,
            format!("{relpath} is a symlink"),
        ));
    }
    Ok(dir)
}

/// A source file (original or incoming text) for a text marker.
///
/// svn requires work item paths below the working copy root, so files elsewhere are staged into
/// `<wc>/.svn/tmp` first and removed by a trailing work item.
#[derive(Debug, Clone)]
pub enum MarkerSource {
    /// A path the work queue runner accepts as a read-only source: a working copy relpath,
    /// `.svn/tmp/<name>`, or (svn 1.14 style) `.svn/pristine/<xx>/<sha1>.svn-base`. It is only
    /// ever read, never removed.
    InWc(String),
    /// A file anywhere (for example in the system temp directory); it is copied into
    /// `.svn/tmp` and the copy is removed by a queued `file-remove`.
    External(PathBuf),
}

/// Result of [`create_text_markers`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMarkers {
    /// Marker relpaths to store in the conflict skel.
    pub conflict: super::TextConflict,
    /// Work items that fill the marker files, in svn's order. Queue them in the same transaction
    /// as the conflict skel.
    pub work_items: Vec<WorkItem>,
}

fn queue_err(e: &crate::workqueue::WorkQueueError) -> SvnError {
    SvnError::new(ErrorCode::UnsafePath, e.to_string())
}

/// Copies `source` (or an external file) into a fresh `.svn/tmp/marker-*` file; returns its relpath.
fn stage_into_tmp(
    wc_root: &Path,
    fill: impl FnOnce(&mut fs::File) -> Result<(), SvnError>,
) -> Result<String, SvnError> {
    let tmp_dir = wc_root.join(crate::db::ADM_DIR).join("tmp");
    fs::create_dir_all(&tmp_dir).map_err(|e| io_err("create directory", &tmp_dir, &e))?;
    let mut tmp = tempfile::Builder::new()
        .prefix("marker-")
        .tempfile_in(&tmp_dir)
        .map_err(|e| io_err("create temp file in", &tmp_dir, &e))?;
    fill(tmp.as_file_mut())?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| io_err("sync", tmp.path(), &e))?;
    let staged = tmp
        .into_temp_path()
        .keep()
        .map_err(|e| io_err("keep", &tmp_dir, &e.error))?;
    Ok(format!(
        "{}/tmp/{}",
        crate::db::ADM_DIR,
        staged
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ))
}

/// Writes the working file at `local_relpath` in repository-normal form (`detranslate_wc_file`
/// with repair) into a fresh `.svn/tmp` file and returns its relpath. A symlink is saved as
/// `link <target>`.
fn stage_detranslated_working_file(
    wc_root: &Path,
    local_relpath: &str,
    info: &TranslateInfo,
) -> Result<String, SvnError> {
    let working = crate::workqueue::paths::resolve_wc_path(wc_root, local_relpath)
        .map_err(|e| queue_err(&e))?;
    let meta = fs::symlink_metadata(&working).map_err(|e| io_err("stat", &working, &e))?;
    stage_into_tmp(wc_root, |out| {
        if meta.file_type().is_symlink() {
            let text = special::read_repo_normal(&working)?;
            out.write_all(&text)
                .map_err(|e| io_err("write", &working, &e))
        } else {
            let mut input = fs::File::open(&working).map_err(|e| io_err("open", &working, &e))?;
            let translator = ContentTranslator::to_repo_normal(info, true)?;
            translate_stream(&mut input, out, translator)?;
            Ok(())
        }
    })
}

/// Creates the three empty text conflict marker files next to `local_relpath` and returns the
/// work items that copy (with keyword expansion and local line endings) the original, incoming
/// and working versions into them, like `preserve_pre_merge_files`.
///
/// This function does the detranslation staging itself: the working file is converted to
/// repository-normal form with `working_props` and written to a fresh `.svn/tmp/marker-*` file,
/// and the queued `file-remove` only ever names that staged temp (and staged copies of
/// [`MarkerSource::External`] files). It can therefore never queue the removal of the user's
/// working file or of any other working copy path. `working_props` selects the old/new property
/// combination to detranslate with (`detranslate_wc_file` cases I-IV are the caller's decision).
///
/// # Partial failure
/// Marker files are created exclusively and empty first, in svn's order. If a later step fails,
/// the empty markers already created stay behind as orphans, exactly as in svn; nothing
/// references them and unique naming skips them next time.
///
/// # Intentional difference from localized svn
/// The label suffixes are always the C-locale names (see [`TextMarkerLabels`]).
pub fn create_text_markers(
    wc_root: &Path,
    local_relpath: &str,
    labels: &TextMarkerLabels,
    original: &MarkerSource,
    their: &MarkerSource,
    working_props: &TranslateInfo,
) -> Result<TextMarkers, SvnError> {
    let (parent, name) = split(local_relpath);
    let dir = abs_dir(wc_root, parent)?;
    // Validate sources before creating anything.
    for source in [original, their] {
        if let MarkerSource::InWc(rel) = source {
            crate::workqueue::paths::resolve_operand(wc_root, rel, true)
                .map_err(|e| queue_err(&e))?;
        }
    }
    let created = [&labels.original, &labels.their, &labels.mine]
        .map(|label| create_unique_file(&dir, name, label).map(|p| marker_relpath(parent, &p)));
    let [original_marker, their_marker, mine_marker] = created;
    let (original_marker, their_marker, mine_marker) =
        (original_marker?, their_marker?, mine_marker?);

    let mut staged_removals = Vec::new();
    let mut stage = |source: &MarkerSource| -> Result<String, SvnError> {
        match source {
            MarkerSource::InWc(rel) => Ok(rel.clone()),
            MarkerSource::External(path) => {
                let rel = stage_into_tmp(wc_root, |out| {
                    let mut input = fs::File::open(path).map_err(|e| io_err("open", path, &e))?;
                    std::io::copy(&mut input, out)
                        .map(|_| ())
                        .map_err(|e| io_err("copy", path, &e))
                })?;
                staged_removals.push(WorkItem::FileRemove {
                    relpath: rel.clone(),
                });
                Ok(rel)
            }
        }
    };
    let original_src = stage(original)?;
    let their_src = stage(their)?;
    let target_src = stage_detranslated_working_file(wc_root, local_relpath, working_props)?;
    let local = local_relpath.to_string();
    let mut work_items = vec![
        WorkItem::FileTranslate {
            local: local.clone(),
            src: original_src,
            dst: original_marker.clone(),
        },
        WorkItem::FileTranslate {
            local: local.clone(),
            src: their_src,
            dst: their_marker.clone(),
        },
        WorkItem::FileTranslate {
            local,
            src: target_src.clone(),
            dst: mine_marker.clone(),
        },
        WorkItem::FileRemove {
            relpath: target_src,
        },
    ];
    // Staged copies of external files go last, like `last_items` in svn.
    work_items.extend(staged_removals);
    Ok(TextMarkers {
        conflict: super::TextConflict {
            original: Some(original_marker),
            mine: Some(mine_marker),
            their: Some(their_marker),
        },
        work_items,
    })
}

fn marker_relpath(parent: &str, created: &Path) -> String {
    join(
        parent,
        &created
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    )
}

/// Result of [`create_prop_marker`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropMarker {
    /// Relpath of the empty `.prej` file just created; prepend it to
    /// [`PropConflict::markers`](super::PropConflict::markers).
    pub relpath: String,
    /// Work item that writes the reject text into it.
    pub work_item: WorkItem,
}

/// Creates the empty property reject file for the node at `local_relpath` (`<file>.prej`, or
/// `dir_conflicts.prej` inside a directory) and returns the `prej-install` work item that fills
/// it (`svn_wc__conflict_create_markers`).
pub fn create_prop_marker(
    wc_root: &Path,
    local_relpath: &str,
    is_dir: bool,
) -> Result<PropMarker, SvnError> {
    let (dir_rel, name) = if is_dir {
        (local_relpath, THIS_DIR_PREJ)
    } else {
        split(local_relpath)
    };
    let dir = abs_dir(wc_root, dir_rel)?;
    let created = create_unique_file(&dir, name, PROP_REJ_EXT)?;
    Ok(PropMarker {
        relpath: marker_relpath(dir_rel, &created),
        work_item: WorkItem::PrejInstall {
            relpath: local_relpath.to_string(),
            legacy_conflict: None,
        },
    })
}

impl super::PropConflict {
    /// Records a marker like svn does: prepended to the list.
    pub fn add_marker(&mut self, relpath: String) {
        self.markers.insert(0, relpath);
    }
}

#[cfg(test)]
mod tests;
