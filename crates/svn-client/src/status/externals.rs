//! `svn:externals` definitions as far as status needs them: which working copy paths they
//! occupy. Fetching and updating externals is outside the MVP; status only has to recognise the
//! directories they live in so they are reported as externals (`X`) and never as unversioned
//! items that a cleanup could delete.

use std::collections::BTreeSet;
use std::path::Path;

use svn_types::{RelPath, Result};
use svn_wc::db::WcDb;

use crate::wc::{inspect, join_rel};

/// Name of the property holding external definitions.
pub const EXTERNALS_PROP: &str = "svn:externals";

/// Splits one definition line into words the way svn does (`svn_cstring_split` after
/// `apr_tokenize_to_argv`): whitespace separates words, single or double quotes group them and a
/// backslash takes the next character literally.
fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
                in_word = true;
            }
            q @ ('"' | '\'') if quote.is_none() => {
                quote = Some(q);
                in_word = true;
            }
            q if quote == Some(q) => quote = None,
            c if c.is_whitespace() && quote.is_none() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

/// True for what svn accepts as the URL of a definition: an absolute URL or one of the relative
/// forms (`^/`, `//`, `/`, `../`).
fn looks_like_url(word: &str) -> bool {
    let word = word.split('@').next().unwrap_or(word);
    word.contains("://")
        || word.starts_with("^/")
        || word.starts_with("//")
        || word.starts_with('/')
        || word.starts_with("../")
}

/// Normalises the target of a definition to a `/`-separated relative path; `None` when it is
/// empty or would leave the defining directory (svn refuses those definitions).
fn target_path(raw: &str) -> Option<String> {
    let raw = raw.replace('\\', "/");
    if raw.starts_with('/') {
        return None;
    }
    let parts: Vec<&str> = raw
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts.is_empty() || parts.contains(&"..") {
        return None;
    }
    Some(parts.join("/"))
}

/// Working copy paths, relative to the directory carrying the property, that an
/// `svn:externals` value defines. Both formats are understood:
///
/// - svn 1.5+ (`[-r N] URL[@PEG] PATH`): the URL comes first, the local path last;
/// - pre-1.5 (`PATH [-r N] URL`): the local path comes first.
///
/// Blank lines and `#` comments are skipped, so are lines svn would reject.
pub fn external_targets(value: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(value);
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut w = words(line);
        // Drop the revision options (`-r N`, `-rN`, `--revision N`).
        let mut i = 0;
        while i < w.len() {
            if w[i] == "-r" || w[i] == "--revision" {
                let end = (i + 2).min(w.len());
                w.drain(i..end);
            } else if w[i].starts_with("-r") && w[i].len() > 2 {
                w.remove(i);
            } else {
                i += 1;
            }
        }
        let [first, second] = w.as_slice() else {
            continue;
        };
        let raw = if looks_like_url(first) {
            second
        } else if looks_like_url(second) {
            first
        } else {
            continue;
        };
        if let Some(t) = target_path(raw) {
            out.push(t);
        }
    }
    out
}

/// Working copy paths occupied by externals: the rows of the `EXTERNALS` table (written by the
/// svn that fetched them) and the targets of `svn:externals` definitions (pristine or local) on
/// versioned directories. The property is read too because a working copy whose externals were
/// fetched by another client may lack rows; recognising a defined path that svn would show as
/// `?` errs on the side of never deleting an external.
#[derive(Debug, Default, Clone)]
pub struct ExternalPaths(BTreeSet<String>);

impl ExternalPaths {
    /// The recorded externals of the working copy plus the definitions on the ancestors of
    /// `path` (`path` itself and what is below it are added with
    /// [`ExternalPaths::add_defined_in`] while walking).
    pub fn for_path(db: &WcDb, dir: &RelPath) -> Result<Self> {
        let mut me = Self(
            db.externals_under(&RelPath::root())?
                .into_iter()
                .map(|e| e.local_relpath.as_str().to_owned())
                .collect(),
        );
        let mut cur = dir.parent();
        while let Some(p) = cur {
            if db.read_node(&p)?.is_some() {
                me.add_defined_in(db, &p)?;
            }
            cur = p.parent();
        }
        Ok(me)
    }

    /// Adds the targets defined by the `svn:externals` property of the versioned directory
    /// `dir`, both the committed and the local value.
    pub fn add_defined_in(&mut self, db: &WcDb, dir: &RelPath) -> Result<()> {
        for props in [db.pristine_props(dir)?, db.actual_props(dir)?] {
            let Some(value) = inspect::prop_value(&props, EXTERNALS_PROP) else {
                continue;
            };
            for t in external_targets(value) {
                if let Ok(t) = RelPath::new(&t)
                    && let Ok(full) = join_rel(dir, &t)
                {
                    self.0.insert(full.as_str().to_owned());
                }
            }
        }
        Ok(())
    }

    /// True when `rel` is an external or an unversioned directory leading to one (svn's
    /// `is_external_path`).
    pub fn covers(&self, rel: &RelPath) -> bool {
        let p = rel.as_str();
        if p.is_empty() {
            return false;
        }
        if self.0.contains(p) {
            return true;
        }
        let prefix = format!("{p}/");
        self.0
            .range(prefix.clone()..)
            .next()
            .is_some_and(|e| e.starts_with(&prefix))
    }
}

/// True when `abs` is a directory holding its own administrative directory: the root of
/// another working copy (an external or a separate checkout nested inside this one).
pub fn is_wc_root_dir(abs: &Path) -> bool {
    abs.symlink_metadata().is_ok_and(|m| m.is_dir())
        && abs
            .join(svn_wc::db::ADM_DIR)
            .symlink_metadata()
            .is_ok_and(|m| m.is_dir())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn t(v: &str) -> Vec<String> {
        external_targets(v.as_bytes())
    }

    #[test]
    fn new_format() {
        assert_eq!(t("^/lib ext"), ["ext"]);
        assert_eq!(t("-r 5 http://h/r/lib@3 a/b"), ["a/b"]);
        assert_eq!(t("-r5 ../x 'sp ace'"), ["sp ace"]);
        assert_eq!(t("//h/r/x \"q d\""), ["q d"]);
        assert_eq!(t("/r/x with\\ space"), ["with space"]);
    }

    #[test]
    fn old_format() {
        assert_eq!(t("ext http://h/r/lib"), ["ext"]);
        assert_eq!(t("third/party -r 21 http://h/r/x"), ["third/party"]);
        assert_eq!(t("\"sp ace\" -r2 file:///r/lib"), ["sp ace"]);
    }

    #[test]
    fn comments_blank_and_bad_lines_are_skipped() {
        assert_eq!(
            t("# c\n\n^/a one\nnot-a-definition\n^/b ../escape\n^/c /abs\n^/d two/\r\n"),
            ["one", "two"]
        );
    }

    #[test]
    fn covers_externals_and_their_ancestors() {
        let mut e = ExternalPaths::default();
        e.0.insert("deep/x/ext".into());
        e.0.insert("lib".into());
        let r = |s: &str| RelPath::new(s).expect("path");
        assert!(e.covers(&r("lib")));
        assert!(e.covers(&r("deep")));
        assert!(e.covers(&r("deep/x")));
        assert!(!e.covers(&r("deep/y")));
        assert!(!e.covers(&r("li")));
        assert!(!e.covers(&r("lib2")));
    }
}
