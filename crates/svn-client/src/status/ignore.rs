//! Ignore patterns: svn's default `global-ignores` and `svn:ignore` properties (fnmatch style).

/// svn's built-in `global-ignores`.
pub const DEFAULT_GLOBAL_IGNORES: &str = "*.o *.lo *.la *.al .libs *.so *.so.[0-9]* *.a *.pyc *.pyo \
    __pycache__ *.rej *~ #*# .#* .*.swp .DS_Store [Tt]humbs.db";

/// Matches `name` against a shell-style `pattern` supporting `*`, `?`, `[set]` and `[!set]`.
pub fn fnmatch(pattern: &str, name: &str) -> bool {
    fn inner(p: &[char], n: &[char]) -> bool {
        match p.split_first() {
            None => n.is_empty(),
            Some(('*', rest)) => (0..=n.len()).any(|i| inner(rest, &n[i..])),
            Some(('?', rest)) => !n.is_empty() && inner(rest, &n[1..]),
            Some(('[', rest)) => {
                let Some(close) = rest.iter().skip(1).position(|c| *c == ']').map(|i| i + 1) else {
                    return !n.is_empty() && n[0] == '[' && inner(rest, &n[1..]);
                };
                let (set, after) = (&rest[..close], &rest[close + 1..]);
                let Some((&c, tail)) = n.split_first() else {
                    return false;
                };
                let (negate, set) = match set.split_first() {
                    Some(('!' | '^', s)) => (true, s),
                    _ => (false, set),
                };
                let mut matched = false;
                let mut i = 0;
                while i < set.len() {
                    if i + 2 < set.len() && set[i + 1] == '-' {
                        matched |= set[i] <= c && c <= set[i + 2];
                        i += 3;
                    } else {
                        matched |= set[i] == c;
                        i += 1;
                    }
                }
                matched != negate && inner(after, tail)
            }
            Some((&c, rest)) => n.first() == Some(&c) && inner(rest, &n[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    inner(&p, &n)
}

/// Patterns from a whitespace separated `global-ignores` value.
pub fn split_global(value: &str) -> Vec<String> {
    value.split_whitespace().map(str::to_owned).collect()
}

/// Patterns from an `svn:ignore` property value (one per line).
pub fn split_property(value: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(value)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// True when any pattern matches `name`.
pub fn is_ignored(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|p| fnmatch(p, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_features() {
        assert!(fnmatch("*.o", "a.o"));
        assert!(!fnmatch("*.o", "a.c"));
        assert!(fnmatch("?.txt", "a.txt"));
        assert!(fnmatch("[Tt]humbs.db", "thumbs.db"));
        assert!(fnmatch("*.so.[0-9]*", "lib.so.1.2"));
        assert!(!fnmatch("[!a]x", "ax"));
        assert!(fnmatch("#*#", "#tmp#"));
    }

    #[test]
    fn default_list_ignores_editor_leftovers() {
        let p = split_global(DEFAULT_GLOBAL_IGNORES);
        assert!(is_ignored(&p, ".DS_Store"));
        assert!(is_ignored(&p, "file.txt~"));
        assert!(!is_ignored(&p, "main.rs"));
    }
}
