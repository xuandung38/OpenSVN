//! Skel codec for the canonical Subversion skel encoding (track A).
//!
//! Grammar, verified against `libsvn_subr/skel.c` at tags 1.14.5 and 1.15.0 (identical):
//!
//! ```text
//! skel     := list | atom
//! list     := '(' (ws* skel)* ws* ')'
//! atom     := implicit | explicit
//! implicit := name-start (any byte that is not whitespace or a paren)*   ; terminated by ws / paren / end
//! explicit := digits ws bytes[digits]                                    ; e.g. `5 a b(c`
//! ```
//!
//! Byte classes are locale independent: whitespace is `\t \n \f \r ' '`, name-start is an ASCII
//! letter, "paren" class is `( ) [ ]` (the square brackets only matter for the implicit atom
//! terminator and for deciding when the writer must use the explicit form).
//!
//! The writer emits the *canonical* form so output is byte-identical to svn: an atom uses the
//! implicit form iff it is non-empty, shorter than 100 bytes, starts with a letter and contains no
//! whitespace/paren-class byte; otherwise `<len> <bytes>`. Lists are `(a b c)` with single spaces.
//!
//! Parsing is lenient exactly where svn is (extra whitespace inside lists, leading zeros in
//! lengths) and rejects everything else, including nesting deeper than [`MAX_DEPTH`].

use svn_types::{ErrorCode, Result, SvnError};

/// Maximum list nesting accepted by the parser (svn imposes none; real skels nest < 10).
pub const MAX_DEPTH: usize = 512;

/// Atoms of this length or longer always use the explicit-length form.
const IMPLICIT_MAX_LEN: usize = 100;

/// A parsed skel: a byte-string atom or a list of skels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skel {
    /// Uninterpreted bytes.
    Atom(Vec<u8>),
    /// Ordered children.
    List(Vec<Skel>),
}

/// Byte classes from the `skel_char_type` table in skel.c.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Class {
    Nothing,
    Space,
    Digit,
    Paren,
    Name,
}

fn class(b: u8) -> Class {
    match b {
        b'\t' | b'\n' | 0x0c | b'\r' | b' ' => Class::Space,
        b'0'..=b'9' => Class::Digit,
        b'(' | b')' | b'[' | b']' => Class::Paren,
        b'A'..=b'Z' | b'a'..=b'z' => Class::Name,
        _ => Class::Nothing,
    }
}

fn malformed(what: &str) -> SvnError {
    SvnError::new(ErrorCode::Corrupt, format!("malformed skel: {what}"))
}

impl Skel {
    /// Builds an atom from any byte string.
    pub fn atom(bytes: impl Into<Vec<u8>>) -> Self {
        Skel::Atom(bytes.into())
    }

    /// Builds a list.
    pub fn list(children: impl Into<Vec<Skel>>) -> Self {
        Skel::List(children.into())
    }

    /// Atom bytes, or `None` for a list.
    pub fn as_atom(&self) -> Option<&[u8]> {
        match self {
            Skel::Atom(b) => Some(b),
            Skel::List(_) => None,
        }
    }

    /// Children, or `None` for an atom.
    pub fn as_list(&self) -> Option<&[Skel]> {
        match self {
            Skel::List(c) => Some(c),
            Skel::Atom(_) => None,
        }
    }

    /// Parses a complete skel. Trailing whitespace is tolerated, other trailing bytes are an error.
    pub fn parse(data: &[u8]) -> Result<Skel> {
        let (skel, used) = Self::parse_prefix(data)?;
        if data[used..].iter().any(|b| class(*b) != Class::Space) {
            return Err(malformed("trailing bytes after skel"));
        }
        Ok(skel)
    }

    /// Parses one skel from the start of `data` and returns it with the number of bytes consumed
    /// (what svn's `svn_skel__parse` does: it ignores whatever follows).
    pub fn parse_prefix(data: &[u8]) -> Result<(Skel, usize)> {
        parse_at(data, 0, 0)
    }

    /// Serializes in the canonical form used by svn.
    pub fn unparse(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.unparse_into(&mut out);
        out
    }

    fn unparse_into(&self, out: &mut Vec<u8>) {
        match self {
            Skel::Atom(bytes) => {
                if use_implicit(bytes) {
                    out.extend_from_slice(bytes);
                } else {
                    out.extend_from_slice(bytes.len().to_string().as_bytes());
                    out.push(b' ');
                    out.extend_from_slice(bytes);
                }
            }
            Skel::List(children) => {
                out.push(b'(');
                for (i, child) in children.iter().enumerate() {
                    if i > 0 {
                        out.push(b' ');
                    }
                    child.unparse_into(out);
                }
                out.push(b')');
            }
        }
    }
}

fn use_implicit(bytes: &[u8]) -> bool {
    match bytes.split_first() {
        Some((first, rest)) => {
            bytes.len() < IMPLICIT_MAX_LEN
                && class(*first) == Class::Name
                && rest
                    .iter()
                    .all(|b| !matches!(class(*b), Class::Space | Class::Paren))
        }
        None => false,
    }
}

/// Parses the skel starting at `data[pos]`; returns it and the index just past it.
fn parse_at(data: &[u8], pos: usize, depth: usize) -> Result<(Skel, usize)> {
    let Some(&c) = data.get(pos) else {
        return Err(malformed("empty input"));
    };
    if c == b'(' {
        return parse_list(data, pos, depth);
    }
    if class(c) == Class::Name {
        let mut end = pos + 1;
        while end < data.len() && !matches!(class(data[end]), Class::Space | Class::Paren) {
            end += 1;
        }
        return Ok((Skel::Atom(data[pos..end].to_vec()), end));
    }
    parse_explicit(data, pos)
}

fn parse_list(data: &[u8], pos: usize, depth: usize) -> Result<(Skel, usize)> {
    if depth >= MAX_DEPTH {
        return Err(malformed("nesting too deep"));
    }
    let mut cur = pos + 1;
    let mut children = Vec::new();
    loop {
        while cur < data.len() && class(data[cur]) == Class::Space {
            cur += 1;
        }
        match data.get(cur) {
            None => return Err(malformed("unterminated list")),
            Some(b')') => return Ok((Skel::List(children), cur + 1)),
            Some(_) => {
                let (child, next) = parse_at(data, cur, depth + 1)?;
                children.push(child);
                cur = next;
            }
        }
    }
}

fn parse_explicit(data: &[u8], pos: usize) -> Result<(Skel, usize)> {
    let digits_end = pos
        + data[pos..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
    if digits_end == pos {
        return Err(malformed("expected list, name or length"));
    }
    let remaining = data.len() - pos;
    let mut len: usize = 0;
    for b in &data[pos..digits_end] {
        len = len
            .checked_mul(10)
            .and_then(|v| v.checked_add(usize::from(b - b'0')))
            .filter(|v| *v <= remaining)
            .ok_or_else(|| malformed("atom length overflow"))?;
    }
    if data.get(digits_end).map(|b| class(*b)) != Some(Class::Space) {
        return Err(malformed("missing space after atom length"));
    }
    let start = digits_end + 1;
    let end = start
        .checked_add(len)
        .filter(|e| *e <= data.len())
        .ok_or_else(|| malformed("atom shorter than its length"))?;
    Ok((Skel::Atom(data[start..end].to_vec()), end))
}

/// One property: `(name, value)` bytes.
pub type PropPair = (Vec<u8>, Vec<u8>);

/// Interprets a skel as a property list `(name value name value ...)`.
pub fn parse_proplist(skel: &Skel) -> Result<Vec<PropPair>> {
    let children = skel
        .as_list()
        .filter(|c| c.len() % 2 == 0)
        .ok_or_else(|| malformed("proplist is not an even-length list"))?;
    children
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[k, v]| match (k.as_atom(), v.as_atom()) {
            (Some(k), Some(v)) => Ok((k.to_vec(), v.to_vec())),
            _ => Err(malformed("proplist contains a non-atom")),
        })
        .collect()
}

/// Builds a property list skel. svn emits hash order; we emit the caller's order so callers can
/// sort for determinism.
pub fn unparse_proplist(props: &[PropPair]) -> Skel {
    Skel::List(
        props
            .iter()
            .flat_map(|(k, v)| [Skel::Atom(k.clone()), Skel::Atom(v.clone())])
            .collect(),
    )
}

/// Inherited properties: `(path-or-url, proplist)` pairs.
pub type InheritedProps = Vec<(Vec<u8>, Vec<PropPair>)>;

/// Interprets a skel as `(path (proplist) path (proplist) ...)`.
pub fn parse_iprops(skel: &Skel) -> Result<InheritedProps> {
    let children = skel
        .as_list()
        .filter(|c| c.len() % 2 == 0)
        .ok_or_else(|| malformed("iprops is not an even-length list"))?;
    children
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[path, props]| {
            let path = path
                .as_atom()
                .ok_or_else(|| malformed("iprops path is not an atom"))?;
            Ok((path.to_vec(), parse_proplist(props)?))
        })
        .collect()
}

/// Builds an inherited-properties skel.
pub fn unparse_iprops(iprops: &InheritedProps) -> Skel {
    Skel::List(
        iprops
            .iter()
            .flat_map(|(path, props)| [Skel::Atom(path.clone()), unparse_proplist(props)])
            .collect(),
    )
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn rt(text: &str) -> String {
        let s = Skel::parse(text.as_bytes()).expect("parse");
        String::from_utf8(s.unparse()).expect("utf8")
    }

    #[test]
    fn implicit_and_explicit_atoms() {
        assert_eq!(rt("(file-install a.txt)"), "(file-install a.txt)");
        // starts with digit / contains space or paren / empty -> explicit
        assert_eq!(rt("(3 1.0 5 a b(c 0 )"), "(3 1.0 5 a b(c 0 )");
        // non-canonical input is normalised by the writer
        assert_eq!(rt("(  p   one )"), "(p one)");
        assert_eq!(rt("(3 abc)"), "(abc)");
        // '[' is paren-class: forces explicit form
        assert_eq!(rt("(3 a[b)"), "(3 a[b)");
    }

    #[test]
    fn long_atom_uses_explicit_form() {
        let a = "a".repeat(100);
        let out = Skel::atom(a.as_bytes()).unparse();
        assert_eq!(out, format!("100 {a}").into_bytes());
        let b = "a".repeat(99);
        assert_eq!(Skel::atom(b.as_bytes()).unparse(), b.into_bytes());
    }

    #[test]
    fn atom_may_contain_binary_and_newlines() {
        let s = Skel::list(vec![Skel::atom(b"x\n\0y".to_vec())]);
        let bytes = s.unparse();
        assert_eq!(bytes, b"(4 x\n\0y)".to_vec());
        assert_eq!(Skel::parse(&bytes).expect("parse"), s);
    }

    #[test]
    fn empty_list_and_nesting() {
        assert_eq!(rt("()"), "()");
        assert_eq!(rt("((a) (b (c)) ())"), "((a) (b (c)) ())");
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "",
            "(",
            "(a",
            "5 abc",
            "3",
            "3x",
            "99999999999999999999999 a",
            "-1 a",
            "(%)",
            "a b",
            ")",
        ] {
            assert!(Skel::parse(bad.as_bytes()).is_err(), "{bad:?} must fail");
        }
        let deep = "(".repeat(MAX_DEPTH + 1) + &")".repeat(MAX_DEPTH + 1);
        assert!(Skel::parse(deep.as_bytes()).is_err());
    }

    #[test]
    fn prefix_parse_ignores_trailing_like_svn() {
        let (s, used) = Skel::parse_prefix(b"(a b) trailing").expect("prefix");
        assert_eq!(used, 5);
        assert_eq!(s.unparse(), b"(a b)".to_vec());
        assert!(Skel::parse(b"(a b) trailing").is_err());
        assert!(Skel::parse(b"(a b) \n").is_ok());
    }

    #[test]
    fn proplist_roundtrip_and_validation() {
        let props = vec![
            (b"svn:keywords".to_vec(), b"Id Rev".to_vec()),
            (b"custom".to_vec(), b"va lue(with) parens".to_vec()),
        ];
        let skel = unparse_proplist(&props);
        let bytes = skel.unparse();
        assert_eq!(
            bytes,
            b"(svn:keywords 6 Id Rev custom 19 va lue(with) parens)".to_vec()
        );
        let back = parse_proplist(&Skel::parse(&bytes).expect("parse")).expect("props");
        assert_eq!(back, props);
        assert!(parse_proplist(&Skel::parse(b"(a)").expect("p")).is_err());
        assert!(parse_proplist(&Skel::parse(b"(a (b))").expect("p")).is_err());
        assert!(parse_proplist(&Skel::atom(b"a".to_vec())).is_err());
        assert!(
            parse_proplist(&Skel::parse(b"()").expect("p"))
                .expect("empty")
                .is_empty()
        );
    }

    #[test]
    fn iprops_roundtrip() {
        let ip: InheritedProps = vec![(b"trunk".to_vec(), vec![(b"p".to_vec(), b"v".to_vec())])];
        let bytes = unparse_iprops(&ip).unparse();
        assert_eq!(bytes, b"(trunk (p v))".to_vec());
        assert_eq!(
            parse_iprops(&Skel::parse(&bytes).expect("p")).expect("ip"),
            ip
        );
        assert!(parse_iprops(&Skel::parse(b"(trunk)").expect("p")).is_err());
    }
}
