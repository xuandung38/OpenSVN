//! Minimal parser/canonicaliser for Subversion "skels", the list syntax wc.db uses for
//! property lists (`NODES.properties`, `ACTUAL_NODE.properties`, `dav_cache`, `inherited_props`).
//!
//! svn serialises a property hash in APR hash order, which changes from process to process, so
//! the same logical properties can have different bytes. [`canonical`] renders a skel with every
//! hash-like list sorted by name so equal content compares equal.
//!
//! Grammar: `( item item ... )`; an item is a list, an *implicit* atom (starts with a letter,
//! no whitespace or parens), or an *explicit* atom `<decimal length> <bytes>`.

use std::fmt::Write as _;

/// Parsed skel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skel {
    /// Byte string.
    Atom(Vec<u8>),
    /// Nested list.
    List(Vec<Skel>),
}

/// Parse a complete skel (trailing whitespace allowed). `None` when malformed.
pub fn parse(data: &[u8]) -> Option<Skel> {
    let (skel, pos) = parse_item(data, skip_ws(data, 0))?;
    (skip_ws(data, pos) == data.len()).then_some(skel)
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

fn skip_ws(data: &[u8], mut pos: usize) -> usize {
    while data.get(pos).copied().is_some_and(is_ws) {
        pos += 1;
    }
    pos
}

fn parse_item(data: &[u8], pos: usize) -> Option<(Skel, usize)> {
    match *data.get(pos)? {
        b'(' => {
            let mut items = Vec::new();
            let mut p = skip_ws(data, pos + 1);
            loop {
                if *data.get(p)? == b')' {
                    return Some((Skel::List(items), p + 1));
                }
                let (item, next) = parse_item(data, p)?;
                items.push(item);
                p = skip_ws(data, next);
            }
        }
        b')' => None,
        first => {
            let end = data[pos..]
                .iter()
                .position(|b| is_ws(*b) || *b == b'(' || *b == b')')
                .map_or(data.len(), |i| pos + i);
            if first.is_ascii_digit() {
                // Explicit atom: length, exactly one whitespace byte, then that many bytes.
                let len: usize = std::str::from_utf8(&data[pos..end]).ok()?.parse().ok()?;
                let start = end + 1;
                if !is_ws(*data.get(end)?) {
                    return None;
                }
                let stop = start.checked_add(len)?;
                Some((Skel::Atom(data.get(start..stop)?.to_vec()), stop))
            } else {
                Some((Skel::Atom(data[pos..end].to_vec()), end))
            }
        }
    }
}

/// Canonical text for a skel blob; falls back to hex when the blob is not a valid skel.
/// Lists that look like hashes (even length, every name position an atom) have their pairs
/// sorted by name, recursively.
pub fn canonical(data: &[u8]) -> String {
    match parse(data) {
        Some(skel) => render(&skel),
        None => hex(data),
    }
}

fn hex(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 2 + 2);
    s.push_str("x'");
    for b in data {
        let _ = write!(s, "{b:02x}");
    }
    s.push('\'');
    s
}

fn render_atom(a: &[u8]) -> String {
    if !a.is_empty() && a.iter().all(|b| b.is_ascii_graphic()) {
        String::from_utf8_lossy(a).into_owned()
    } else {
        hex(a)
    }
}

fn render(skel: &Skel) -> String {
    match skel {
        Skel::Atom(a) => render_atom(a),
        Skel::List(items) => {
            let hash_like = !items.is_empty()
                && items.len() % 2 == 0
                && items.chunks(2).all(|pair| matches!(pair[0], Skel::Atom(_)));
            let mut parts: Vec<(String, String)> = if hash_like {
                items
                    .chunks(2)
                    .map(|p| (render(&p[0]), render(&p[1])))
                    .collect()
            } else {
                items.iter().map(|i| (render(i), String::new())).collect()
            };
            if hash_like {
                parts.sort();
                let body: Vec<String> =
                    parts.into_iter().map(|(k, v)| format!("{k}={v}")).collect();
                format!("{{{}}}", body.join(" "))
            } else {
                let body: Vec<String> = parts.into_iter().map(|(k, _)| k).collect();
                format!("[{}]", body.join(" "))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_implicit_and_explicit_atoms() {
        let s = parse(b"(svn:keywords 6 Id Rev)");
        assert_eq!(
            s,
            Some(Skel::List(vec![
                Skel::Atom(b"svn:keywords".to_vec()),
                Skel::Atom(b"Id Rev".to_vec())
            ]))
        );
        assert_eq!(parse(b"()"), Some(Skel::List(vec![])));
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [&b"(a b"[..], b"a)", b"(5 ab)", b"", b"(a) b"] {
            assert!(parse(bad).is_none(), "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn property_order_does_not_change_canonical_form() {
        let a = b"(svn:eol-style native svn:keywords 6 Id Rev svn:needs-lock 1 *)";
        let b = b"(svn:needs-lock 1 * svn:eol-style native svn:keywords 6 Id Rev)";
        assert_ne!(a.as_slice(), b.as_slice());
        assert_eq!(canonical(a), canonical(b));
        let different = b"(svn:needs-lock 1 * svn:eol-style CRLF svn:keywords 6 Id Rev)";
        assert_ne!(canonical(a), canonical(different));
    }

    #[test]
    fn nested_hashes_are_sorted_and_invalid_blobs_fall_back_to_hex() {
        let a = b"(b (y 1 1 x 1 2) a 1 1)";
        let b = b"(a 1 1 b (x 1 2 y 1 1))";
        assert_eq!(canonical(a), canonical(b));
        assert_eq!(canonical(b"(oops"), "x'286f6f7073'");
    }
}
