//! Comparison helpers: file-tree snapshots and normalised `svn --xml` output.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

/// One entry of a working-copy tree snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeEntry {
    /// Directory.
    Dir,
    /// Regular file with its bytes, exec bit and read-only state (`svn:needs-lock` files are
    /// read-only until locked).
    File {
        /// File content.
        content: Vec<u8>,
        /// True when the owner/group/other exec bit is set (always false on non-Unix hosts).
        executable: bool,
        /// True when no write bit is set (Windows: the read-only attribute).
        readonly: bool,
    },
    /// Symlink and its target.
    Symlink(String),
}

/// Relative-path (with `/` separators) to entry map of a directory, skipping `.svn`.
pub type TreeSnapshot = BTreeMap<String, TreeEntry>;

/// Snapshot `root`, excluding the `.svn` administrative directory.
pub fn snapshot_tree(root: &Path) -> Result<TreeSnapshot> {
    let mut out = TreeSnapshot::new();
    walk(root, root, &mut out)?;
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut TreeSnapshot) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .with_context(|| format!("read_dir {}", dir.display()))?
        .collect::<std::io::Result<_>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        if entry.file_name() == ".svn" {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .with_context(|| format!("{} is outside {}", path.display(), root.display()))?
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            let target = fs::read_link(&path)?.to_string_lossy().into_owned();
            out.insert(rel, TreeEntry::Symlink(target));
        } else if ft.is_dir() {
            out.insert(rel, TreeEntry::Dir);
            walk(root, &path, out)?;
        } else {
            let content = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            let (executable, readonly) = file_mode_bits(&entry)?;
            out.insert(
                rel,
                TreeEntry::File {
                    content,
                    executable,
                    readonly,
                },
            );
        }
    }
    Ok(())
}

/// `(executable, readonly)` for a directory entry. The write bit is reduced to a boolean
/// because umask differs between the container and the host.
#[cfg(unix)]
fn file_mode_bits(entry: &fs::DirEntry) -> Result<(bool, bool)> {
    use std::os::unix::fs::PermissionsExt;
    let mode = entry.metadata()?.permissions().mode();
    Ok((mode & 0o111 != 0, mode & 0o222 == 0))
}

#[cfg(not(unix))]
fn file_mode_bits(entry: &fs::DirEntry) -> Result<(bool, bool)> {
    Ok((false, entry.metadata()?.permissions().readonly()))
}

/// Human-readable differences between two snapshots (empty when identical).
pub fn diff_trees(left: &TreeSnapshot, right: &TreeSnapshot) -> Vec<String> {
    let mut diffs = Vec::new();
    for (path, l) in left {
        match right.get(path) {
            None => diffs.push(format!("only in left: {path}")),
            Some(r) if r != l => {
                diffs.push(format!("differs: {path} ({})", describe_difference(l, r)))
            }
            Some(_) => {}
        }
    }
    for path in right.keys().filter(|p| !left.contains_key(*p)) {
        diffs.push(format!("only in right: {path}"));
    }
    diffs
}

fn describe_difference(l: &TreeEntry, r: &TreeEntry) -> String {
    match (l, r) {
        (
            TreeEntry::File {
                content: lc,
                executable: le,
                readonly: lr,
            },
            TreeEntry::File {
                content: rc,
                executable: re,
                readonly: rr,
            },
        ) => {
            let mut parts = Vec::new();
            if lc != rc {
                parts.push(format!("content {} vs {} bytes", lc.len(), rc.len()));
            }
            if le != re {
                parts.push(format!("exec bit {le} vs {re}"));
            }
            if lr != rr {
                parts.push(format!("read-only {lr} vs {rr}"));
            }
            parts.join(", ")
        }
        _ => "entry kind or target".to_string(),
    }
}

/// XML elements derived from local file mtimes. Server-sourced values (`<date>`, lock
/// `<created>`/`<expires>`/`<token>`) are identical across working copies and stay compared.
const VOLATILE_ELEMENTS: &[&str] = &["text-updated", "prop-updated"];

/// Normalise `svn status|info --xml` output for comparison: drop mtime-derived elements, replace
/// the absolute working-copy path with `{WC}`, sort attributes inside each tag (svn emits them
/// in per-process hash order, which differs run to run), and emit one token per line.
pub fn normalize_xml(xml: &str, wc_abspath: &str) -> String {
    let mut text = xml.replace(wc_abspath, "{WC}");
    for name in VOLATILE_ELEMENTS {
        text = remove_element(&text, name);
    }
    let mut out: Vec<String> = Vec::new();
    let mut rest = text.as_str();
    while let Some(start) = rest.find('<') {
        let chars = rest[..start].trim();
        if !chars.is_empty() {
            out.push(chars.to_string());
        }
        let (tag, after) = split_tag(&rest[start..]);
        out.push(canonical_tag(tag));
        rest = after;
    }
    let tail = rest.trim();
    if !tail.is_empty() {
        out.push(tail.to_string());
    }
    sort_entry_runs(&mut out);
    out.join("\n")
}

/// Sort each contiguous run of `<entry ...>...</entry>` blocks by content. Old svn (1.8) lists
/// `info --recursive` entries in per-process hash order; blocks start with their `path` attribute
/// so sorting the block text orders them by path.
fn sort_entry_runs(tokens: &mut Vec<String>) {
    let mut result: Vec<String> = Vec::with_capacity(tokens.len());
    let mut run: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for tok in tokens.drain(..) {
        if let Some(block) = current.as_mut() {
            block.push('\n');
            block.push_str(&tok);
            if tok == "</entry>" {
                run.extend(current.take());
            }
        } else if tok.starts_with("<entry ") || tok == "<entry>" {
            current = Some(tok);
        } else {
            run.sort();
            result.append(&mut run);
            result.push(tok);
        }
    }
    run.sort();
    result.append(&mut run);
    if let Some(unfinished) = current {
        result.push(unfinished);
    }
    *tokens = result;
}

/// Split off the leading `<...>` tag, honouring quoted attribute values.
fn split_tag(s: &str) -> (&str, &str) {
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return (&s[..=i], &s[i + 1..]),
            _ => {}
        }
    }
    (s, "")
}

/// Rewrite a tag with whitespace collapsed and attributes sorted by name.
fn canonical_tag(tag: &str) -> String {
    let inner = tag.trim_start_matches('<').trim_end_matches('>');
    let (inner, self_close) = inner
        .strip_suffix('/')
        .map_or((inner, false), |i| (i, true));
    let inner = inner.trim();
    let name_end = inner.find(char::is_whitespace).unwrap_or(inner.len());
    let (name, attrs_src) = inner.split_at(name_end);
    let mut attrs: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in attrs_src.chars() {
        match (quote, c) {
            (None, '"' | '\'') => {
                quote = Some(c);
                cur.push(c);
            }
            (Some(q), c) if c == q => {
                quote = None;
                cur.push(c);
                attrs.push(std::mem::take(&mut cur));
            }
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() {
                    cur.push(' ');
                }
            }
            (_, c) => cur.push(c),
        }
    }
    attrs.sort();
    let mut out = format!("<{name}");
    for a in attrs {
        out.push(' ');
        out.push_str(a.trim());
    }
    out.push_str(if self_close { "/>" } else { ">" });
    out
}

/// Remove every `<name ...>...</name>` and `<name/>` occurrence (no nested same-name elements
/// occur in svn's status/info schema).
fn remove_element(text: &str, name: &str) -> String {
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(&open) {
        // `<date` must be followed by `>`, `/` or whitespace so `<dated>` is not matched.
        let after = rest[start + open.len()..].chars().next();
        if !matches!(after, Some('>' | '/' | ' ' | '\t' | '\n' | '\r')) {
            out.push_str(&rest[..start + open.len()]);
            rest = &rest[start + open.len()..];
            continue;
        }
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let Some(tag_end) = tail.find('>') else {
            return out;
        };
        let self_closing = tail[..tag_end].ends_with('/');
        rest = if self_closing {
            &tail[tag_end + 1..]
        } else if let Some(c) = tail.find(&close) {
            &tail[c + close.len()..]
        } else {
            &tail[tag_end + 1..]
        };
    }
    out.push_str(rest);
    out
}

/// Line-level difference summary of two normalised texts (empty when equal).
pub fn diff_text(left: &str, right: &str) -> Vec<String> {
    if left == right {
        return Vec::new();
    }
    let l: Vec<&str> = left.lines().collect();
    let r: Vec<&str> = right.lines().collect();
    let mut diffs = Vec::new();
    for i in 0..l.len().max(r.len()) {
        let (a, b) = (l.get(i).copied(), r.get(i).copied());
        if a != b {
            diffs.push(format!("line {}: left={:?} right={:?}", i + 1, a, b));
            if diffs.len() >= 20 {
                diffs.push("... (truncated)".into());
                break;
            }
        }
    }
    diffs
}

/// Order-insensitive line difference (multiset): lines only on one side, with counts.
/// Used for sorted dumps where a positional diff would show every following line as changed.
pub fn diff_lines_unordered(left: &str, right: &str) -> Vec<String> {
    let mut counts: BTreeMap<&str, i64> = BTreeMap::new();
    for l in left.lines() {
        *counts.entry(l).or_default() += 1;
    }
    for r in right.lines() {
        *counts.entry(r).or_default() -= 1;
    }
    let mut diffs: Vec<String> = counts
        .into_iter()
        .filter(|(_, n)| *n != 0)
        .map(|(line, n)| {
            format!(
                "{} x{}: {line}",
                if n > 0 {
                    "only in left"
                } else {
                    "only in right"
                },
                n.abs()
            )
        })
        .collect();
    if diffs.len() > 20 {
        diffs.truncate(20);
        diffs.push("... (truncated)".into());
    }
    diffs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_snapshot_ignores_svn_dir_and_flags_exec_bit() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        for d in [&a, &b] {
            fs::create_dir_all(d.join(".svn"))?;
            fs::create_dir_all(d.join("sub"))?;
            fs::write(d.join(".svn/junk"), d.to_string_lossy().as_bytes())?;
            fs::write(d.join("sub/f.txt"), b"same")?;
        }
        assert!(diff_trees(&snapshot_tree(&a)?, &snapshot_tree(&b)?).is_empty());

        fs::write(b.join("sub/f.txt"), b"diff!")?;
        fs::write(b.join("extra"), b"x")?;
        let d = diff_trees(&snapshot_tree(&a)?, &snapshot_tree(&b)?);
        assert_eq!(d.len(), 2, "{d:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::write(a.join("sub/f.txt"), b"diff!")?;
            fs::remove_file(b.join("extra"))?;
            fs::set_permissions(b.join("sub/f.txt"), fs::Permissions::from_mode(0o755))?;
            let d = diff_trees(&snapshot_tree(&a)?, &snapshot_tree(&b)?);
            assert_eq!(d.len(), 1);
            assert!(d[0].contains("exec bit"), "{d:?}");

            // needs-lock state: same content and mode otherwise, but one copy is read-only.
            fs::set_permissions(b.join("sub/f.txt"), fs::Permissions::from_mode(0o644))?;
            assert!(diff_trees(&snapshot_tree(&a)?, &snapshot_tree(&b)?).is_empty());
            fs::set_permissions(b.join("sub/f.txt"), fs::Permissions::from_mode(0o444))?;
            let d = diff_trees(&snapshot_tree(&a)?, &snapshot_tree(&b)?);
            assert_eq!(d.len(), 1, "{d:?}");
            assert!(d[0].contains("read-only"), "{d:?}");
        }
        Ok(())
    }

    #[test]
    fn normalize_drops_only_mtime_elements_and_wc_path() {
        let xml = "<info>\n  <wcroot-abspath>/work/s/a</wcroot-abspath>\n  <text-updated>2025-01-01T00:00:00.000000Z</text-updated>\n  <commit revision=\"3\">\n    <date>2024-01-01T00:00:00.000000Z</date>\n  </commit>\n  <lock><created>x</created></lock>\n  <dated>keep</dated>\n</info>";
        let n = normalize_xml(xml, "/work/s/a");
        assert!(n.contains("<wcroot-abspath>\n{WC}\n</wcroot-abspath>"));
        assert!(!n.contains("2025-01-01"), "text-updated is mtime-derived");
        assert!(
            n.contains("2024-01-01"),
            "server commit date must stay compared"
        );
        assert!(n.contains("created"));
        assert!(n.contains("<dated>\nkeep\n</dated>"));
    }

    #[test]
    fn entry_blocks_are_ordered_independently_of_listing_order() {
        let a = "<info><entry kind=\"file\" path=\"b\"><url>u/b</url></entry><entry kind=\"file\" path=\"a\"><url>u/a</url></entry></info>";
        let b = "<info><entry kind=\"file\" path=\"a\"><url>u/a</url></entry><entry kind=\"file\" path=\"b\"><url>u/b</url></entry></info>";
        assert_eq!(normalize_xml(a, "/w"), normalize_xml(b, "/w"));
        let c = b.replace("u/b", "u/changed");
        assert_ne!(normalize_xml(a, "/w"), normalize_xml(&c, "/w"));
    }

    #[test]
    fn normalize_sorts_attributes_and_collapses_tag_whitespace() {
        let a =
            "<entry\n   path=\"x\"\n   kind=\"file\">\n<s item=\"normal\" props=\"none\"/></entry>";
        let b = "<entry kind=\"file\" path=\"x\"><s props=\"none\"\n item=\"normal\"/>\n</entry>";
        assert_eq!(normalize_xml(a, "/w"), normalize_xml(b, "/w"));
        assert!(normalize_xml(a, "/w").contains("<entry kind=\"file\" path=\"x\">"));
        assert_ne!(
            normalize_xml(a, "/w"),
            normalize_xml(&a.replace("\"x\"", "\"y\""), "/w")
        );
    }

    #[test]
    fn unordered_diff_ignores_order_and_counts_duplicates() {
        assert!(diff_lines_unordered("a\nb\nb", "b\na\nb").is_empty());
        let d = diff_lines_unordered("a\nb", "a\nc");
        assert_eq!(d.len(), 2, "{d:?}");
    }

    #[test]
    fn diff_text_reports_changed_lines() {
        assert!(diff_text("a\nb", "a\nb").is_empty());
        let d = diff_text("a\nb", "a\nc");
        assert_eq!(d.len(), 1);
        assert!(d[0].starts_with("line 2"));
    }
}
