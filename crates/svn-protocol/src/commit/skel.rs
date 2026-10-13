//! Skel encoding for the `POST !svn/me` transaction-creation body.
//!
//! A skel is an s-expression of atoms and lists. An atom is written bare when it is a plain
//! name (starts with a letter, then letters, digits or `-._:/@`), otherwise as
//! `<length> <bytes>`. The explicit form is always valid, so the bare form is only used where
//! the reference client uses it.

use bytes::{BufMut, Bytes, BytesMut};

fn is_bare(atom: &[u8]) -> bool {
    atom.first().is_some_and(u8::is_ascii_alphabetic)
        && atom.iter().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b':' | b'/' | b'@')
        })
}

fn push_atom(out: &mut BytesMut, atom: &[u8]) {
    if is_bare(atom) {
        out.put_slice(atom);
    } else {
        out.put_slice(atom.len().to_string().as_bytes());
        out.put_u8(b' ');
        out.put_slice(atom);
    }
}

/// Body of `create-txn` (no revision properties).
pub fn create_txn_body() -> Bytes {
    Bytes::from_static(b"(create-txn)")
}

/// Body of `create-txn-with-props`: `(create-txn-with-props (name value name value ...))`.
pub fn create_txn_with_props_body(revprops: &[(&str, &[u8])]) -> Bytes {
    let mut out = BytesMut::new();
    out.put_slice(b"(create-txn-with-props (");
    for (i, (name, value)) in revprops.iter().enumerate() {
        if i > 0 {
            out.put_u8(b' ');
        }
        push_atom(&mut out, name.as_bytes());
        out.put_u8(b' ');
        push_atom(&mut out, value);
    }
    out.put_slice(b"))");
    out.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atoms_follow_the_reference_encoding() {
        let body = create_txn_with_props_body(&[
            ("svn:txn-client-compat-version", b"1.15.0"),
            ("svn:log", b"props, delete, copy, add"),
            ("svn:author", b"alice"),
        ]);
        assert_eq!(
            body,
            "(create-txn-with-props (svn:txn-client-compat-version 6 1.15.0 svn:log 24 props, delete, copy, add svn:author alice))"
        );
        assert_eq!(
            create_txn_with_props_body(&[]),
            "(create-txn-with-props ())"
        );
        assert_eq!(
            create_txn_with_props_body(&[("svn:log", b"")]),
            "(create-txn-with-props (svn:log 0 ))"
        );
    }
}
