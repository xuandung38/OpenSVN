//! Ignore rules: `fnmatch` matching, `global-ignores` and `svn:ignore` / `svn:global-ignores`.

use svn_types::{RelPath, Result};
use svn_wc::WcDb;

/// Subversion's built-in `global-ignores` default.
pub const DEFAULT_GLOBAL_IGNORES: &[&str] = &[
    "*.o",
    "*.lo",
    "*.la",
    "*.al",
    ".libs",
    "*.so",
    "*.so.[0-9]*",
    "*.a",
    "*.pyc",
    "*.pyo",
    "__pycache__",
    "*.rej",
    "*~",
    "#*#",
    ".#*",
    ".*.swp",
    ".DS_Store",
    "[Tt]humbs.db",
];

/// Shell-style pattern match of a single path segment (`*`, `?`, `[...]`, `\` escape), like
/// APR's `apr_fnmatch` without `FNM_PATHNAME`.
pub fn fnmatch(pattern: &str, name: &str, case_blind: bool) -> bool {
    let p: Vec<char> = if case_blind {
        pattern.to_lowercase().chars().collect()
    } else {
        pattern.chars().collect()
    };
    let n: Vec<char> = if case_blind {
        name.to_lowercase().chars().collect()
    } else {
        name.chars().collect()
    };
    match_from(&p, &n)
}

fn match_from(p: &[char], n: &[char]) -> bool {
    let Some((&pc, prest)) = p.split_first() else {
        return n.is_empty();
    };
    match pc {
        '*' => (0..=n.len()).any(|i| match_from(prest, &n[i..])),
        '?' => !n.is_empty() && match_from(prest, &n[1..]),
        '[' => {
            let Some((&c, nrest)) = n.split_first() else {
                return false;
            };
            match class_end(p) {
                Some(end) => class_matches(&p[1..end], c) && match_from(&p[end + 1..], nrest),
                // An unterminated class matches a literal '['.
                None => c == '[' && match_from(prest, nrest),
            }
        }
        '\\' if !prest.is_empty() => {
            !n.is_empty() && n[0] == prest[0] && match_from(&prest[1..], &n[1..])
        }
        c => !n.is_empty() && n[0] == c && match_from(prest, &n[1..]),
    }
}

/// Index of the `]` closing the class starting at `p[0] == '['`.
fn class_end(p: &[char]) -> Option<usize> {
    let mut i = 1;
    if matches!(p.get(i), Some('!' | '^')) {
        i += 1;
    }
    if p.get(i) == Some(&']') {
        i += 1;
    }
    while i < p.len() {
        if p[i] == ']' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn class_matches(body: &[char], c: char) -> bool {
    let (negate, body) = match body.first() {
        Some('!' | '^') => (true, &body[1..]),
        _ => (false, body),
    };
    let mut hit = false;
    let mut i = 0;
    while i < body.len() {
        if i + 2 < body.len() && body[i + 1] == '-' {
            if body[i] <= c && c <= body[i + 2] {
                hit = true;
            }
            i += 3;
        } else {
            if body[i] == c {
                hit = true;
            }
            i += 1;
        }
    }
    hit != negate
}

/// True when any pattern matches `name`.
pub fn matches_any<S: AsRef<str>>(patterns: &[S], name: &str) -> bool {
    patterns.iter().any(|p| fnmatch(p.as_ref(), name, false))
}

fn prop_patterns(db: &WcDb, dir: &RelPath, prop: &str) -> Result<Vec<String>> {
    Ok(db
        .actual_props(dir)?
        .into_iter()
        .find(|(k, _)| k.as_slice() == prop.as_bytes())
        .map(|(_, v)| {
            String::from_utf8_lossy(&v)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default())
}

/// Whether the unversioned child `name` of versioned directory `dir` is ignored: matched by the
/// configured `global_ignores`, by `svn:global-ignores` of `dir` and its ancestors, or by
/// `svn:ignore` of `dir`.
pub fn is_ignored(db: &WcDb, dir: &RelPath, name: &str, global_ignores: &[String]) -> Result<bool> {
    if matches_any(global_ignores, name) {
        return Ok(true);
    }
    let mut cur = Some(dir.clone());
    while let Some(d) = cur {
        // Ancestors that are not versioned (cannot happen below a versioned dir) are skipped.
        if db.read_node(&d)?.is_some() {
            if matches_any(&prop_patterns(db, &d, "svn:global-ignores")?, name) {
                return Ok(true);
            }
            if &d == dir && matches_any(&prop_patterns(db, &d, "svn:ignore")?, name) {
                return Ok(true);
            }
        }
        cur = d.parent();
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnmatch_basics() {
        assert!(fnmatch("*.o", "a.o", false));
        assert!(!fnmatch("*.o", "a.c", false));
        assert!(fnmatch("[Tt]humbs.db", "thumbs.db", false));
        assert!(fnmatch("*.so.[0-9]*", "lib.so.1.2", false));
        assert!(fnmatch("?x", "ax", false));
        assert!(fnmatch("a[!b]c", "axc", false));
        assert!(!fnmatch("a[!b]c", "abc", false));
        assert!(fnmatch("a[b-d]", "ac", false));
        assert!(fnmatch("*.TXT", "x.txt", true));
        assert!(!fnmatch("*.TXT", "x.txt", false));
        assert!(fnmatch("a\\*b", "a*b", false));
        assert!(!fnmatch("a\\*b", "axb", false));
        assert!(fnmatch("#*#", "#x#", false));
    }
}
