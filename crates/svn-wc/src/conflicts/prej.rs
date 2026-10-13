//! Text of property reject (`.prej`) files, ported from `libsvn_wc/props.c`
//! (`generate_conflict_message`, `prop_conflict_new`, `svn_wc__create_prejfile`; identical in
//! svn 1.14.5 and 1.15.0).
//!
//! For every conflicted property the file holds a sentence describing what went wrong, followed by
//! a three-way merge of the values with conflict markers (or, when a value is binary or the merge
//! has no conflict, the plain values). The wording is always the C-locale English text; real svn
//! passes these sentences through gettext. The `WorkingCopyOps::write_prop_reject_file`
//! implementation writes the returned bytes to a temp file and renames it over the marker.
//!
//! Known approximations, all confined to the *diagnostic text*:
//! * svn orders the properties by APR hash iteration; this port sorts the names bytewise.
//! * The "fuzzy" ASCII rendering of non-ASCII values (`svn_utf__fuzzy_escape`) uses NFKD
//!   decomposition with combining-mark stripping (`é` becomes `e`) and utf8proc's `LUMP` table
//!   (`’` to `'`, dashes to `-`, no-break space to a space); verified against svn 1.15.0
//!   fixtures. Stripping uses the canonical combining class, which equals utf8proc's mark
//!   test for all practical text. A UTF-8-encoded surrogate renders as `?\XX` bytes where svn
//!   prints `{U?D800}`.
//! * The legacy 1.7/1.8 `prej-install` conflict skel is not supported.

use std::fmt::Write as _;

use svn_diff::{ConflictLabels, MergeOptions, merge3};
use svn_types::CancelToken;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::canonical_combining_class;

use super::{Operation, PropConflict};
use crate::skel::PropPair;

/// The four values of one conflicted property, in svn's terms.
#[derive(Debug, Clone, Copy, Default)]
pub struct PropValues<'a> {
    /// The reference value: the pristine value for a merge, the incoming "changed from" value
    /// for an update or switch.
    pub original: Option<&'a [u8]>,
    /// The local value.
    pub mine: Option<&'a [u8]>,
    /// The incoming "changed to" value.
    pub incoming: Option<&'a [u8]>,
    /// The incoming "changed from" value.
    pub incoming_base: Option<&'a [u8]>,
}

/// `svn_io_is_binary_data`.
fn is_binary(data: &[u8]) -> bool {
    if data == [0xEF, 0xBB, 0xBF] {
        return false; // a lone UTF-8 BOM is text
    }
    if data.is_empty() {
        return false;
    }
    if data.contains(&0) {
        return true;
    }
    let odd = data
        .iter()
        .filter(|b| **b < 0x07 || (**b > 0x0D && **b < 0x20) || **b > 0x7F)
        .count();
    (odd * 1000) / data.len() > 850
}

/// `generate_conflict_message`. Branches whose preconditions svn asserts (they cannot happen for
/// a conflict svn itself recorded) fall through to the generic wording instead of aborting.
fn conflict_message(name: &str, v: &PropValues<'_>) -> String {
    let PropValues {
        original,
        mine,
        incoming,
        incoming_base,
    } = *v;
    if incoming_base.is_none() {
        // Attempting to add the value INCOMING.
        return if mine.is_some() {
            format!("Trying to add new property '{name}'\nbut the property already exists.\n")
        } else {
            format!(
                "Trying to add new property '{name}'\nbut the property has been locally deleted.\n"
            )
        };
    }
    if incoming.is_none() {
        // Attempting to delete the value INCOMING_BASE.
        if original.is_none() && mine.is_some() {
            return format!(
                "Trying to delete property '{name}'\nbut the property has been locally added.\n"
            );
        }
        if original == incoming_base {
            if mine.is_some() {
                return format!(
                    "Trying to delete property '{name}'\nbut the property has been locally modified.\n"
                );
            }
        } else if mine.is_none() {
            return format!(
                "Trying to delete property '{name}'\nbut the property has been locally deleted and had a different value.\n"
            );
        }
        return format!(
            "Trying to delete property '{name}'\nbut the local property value is different.\n"
        );
    }
    // Attempting to change the property from INCOMING_BASE to INCOMING.
    if original.is_some() && mine.is_some() && original == mine {
        return format!(
            "Trying to change property '{name}'\nbut the local property value conflicts with the incoming change.\n"
        );
    }
    match (original.is_some(), mine.is_some()) {
        (true, true) => format!(
            "Trying to change property '{name}'\nbut the property has already been locally changed to a different value.\n"
        ),
        (true, false) => format!(
            "Trying to change property '{name}'\nbut the property has been locally deleted.\n"
        ),
        (false, true) => format!(
            "Trying to change property '{name}'\nbut the property has been locally added with a different value.\n"
        ),
        (false, false) => format!(
            "Trying to change property '{name}'\nbut the property does not exist locally.\n"
        ),
    }
}

/// utf8proc's `UTF8PROC_LUMP` mapping, the part svn's fuzzy rendering applies before
/// decomposing: space separators (Zs) to a space, dash punctuation (Pd) and the minus sign to
/// `-`, connector punctuation (Pc) to `_`, plus a few quote, slash, bracket and caret lookalikes.
/// Double quotes are deliberately absent: utf8proc leaves them alone.
fn lump(c: char) -> Option<char> {
    Some(match c as u32 {
        // Zs
        0x00A0 | 0x1680 | 0x2000..=0x200A | 0x202F | 0x205F | 0x3000 => ' ',
        // Apostrophes
        0x2018 | 0x2019 | 0x02BC | 0x02C8 => '\'',
        // Pd and U+2212
        0x058A
        | 0x05BE
        | 0x1400
        | 0x1806
        | 0x2010..=0x2015
        | 0x2E17
        | 0x2E1A
        | 0x2E3A
        | 0x2E3B
        | 0x2E40
        | 0x301C
        | 0x3030
        | 0x30A0
        | 0xFE31
        | 0xFE32
        | 0xFE58
        | 0xFE63
        | 0xFF0D
        | 0x2212 => '-',
        0x2044 | 0x2215 => '/',
        0x2236 => ':',
        0x2039 | 0x2329 | 0x3008 => '<',
        0x203A | 0x232A | 0x3009 => '>',
        0x2216 => '\\',
        0x02C4 | 0x02C6 | 0x2038 | 0x2303 => '^',
        // Pc and U+02CD
        0x203F | 0x2040 | 0x2054 | 0xFE33 | 0xFE34 | 0xFE4D..=0xFE4F | 0xFF3F | 0x02CD => '_',
        0x02CB => '`',
        0x2223 => '|',
        0x223C => '~',
        _ => return None,
    })
}

/// `svn_utf__fuzzy_escape`: an ASCII-only rendering for display.
///
/// Mirrors `utf8proc_decompose` with the flags svn passes (compatibility decomposition, `LUMP`,
/// `STRIPMARK`) followed by svn's own placeholder loop: bytes 1..126 pass through unchanged
/// (tab, form feed, vertical tab, CR and LF included; the newline rewriting flags of utf8proc do
/// not apply to `decompose`), NUL becomes `\0`, invalid bytes become `?\XX`, combining marks are
/// dropped, and every other character, DEL and NEL included, becomes `{U+XXXX}`.
pub fn fuzzy_escape(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while !rest.is_empty() {
        let (valid, invalid_len) = match std::str::from_utf8(rest) {
            Ok(s) => (s, 0),
            Err(e) => {
                // `valid_up_to` is always a char boundary (contract of `Utf8Error`).
                let (good, bad) = rest.split_at(e.valid_up_to());
                (
                    std::str::from_utf8(good).unwrap_or(""),
                    e.error_len().unwrap_or(bad.len()),
                )
            }
        };
        for c in valid.chars() {
            match lump(c) {
                Some(l) => out.push(l),
                None => {
                    for d in std::iter::once(c).nfkd() {
                        if canonical_combining_class(d) != 0 {
                            continue;
                        }
                        match d as u32 {
                            0 => out.push_str("\\0"),
                            1..=126 => out.push(d),
                            cp if cp > 0xFFFF => {
                                let _ = write!(out, "{{U+{cp:06X}}}");
                            }
                            cp => {
                                let _ = write!(out, "{{U+{cp:04X}}}");
                            }
                        }
                    }
                }
            }
        }
        rest = &rest[valid.len()..];
        for b in rest.iter().take(invalid_len) {
            let _ = write!(out, "?\\{b:02X}");
        }
        rest = &rest[invalid_len.min(rest.len())..];
    }
    out
}

/// `prop_conflict_new`: the description of one conflicted property.
pub fn conflict_description(name: &str, v: &PropValues<'_>) -> Vec<u8> {
    let mut out = conflict_message(name, v).into_bytes();
    let mine = v.mine.unwrap_or_default();
    let incoming = v.incoming.unwrap_or_default();
    let incoming_base = v.incoming_base.unwrap_or_default();
    let (b_base, b_mine, b_in) = (
        is_binary(incoming_base),
        is_binary(mine),
        is_binary(incoming),
    );

    if !(b_base || b_mine || b_in) {
        // Left = incoming "changed from", modified = mine, latest = incoming "changed to".
        let opts = MergeOptions {
            labels: ConflictLabels {
                mine: "(local property value)".into(),
                base: "(incoming 'changed from' value)".into(),
                theirs: "(incoming 'changed to' value)".into(),
            },
            ..MergeOptions::default()
        };
        // svn decides on the raw values and only renders the ASCII-fuzzed text, so values that
        // differ just in characters that fuzz to the same ASCII still conflict. The merged
        // output consists of raw value lines and ASCII marker lines, so fuzzing the whole
        // output renders the same text.
        let merged = merge3(incoming_base, mine, incoming, &opts, &CancelToken::new());
        if let Ok(result) = merged
            && !result.conflicts.is_empty()
        {
            out.extend_from_slice(fuzzy_escape(&result.text).as_bytes());
            return out;
        }
    }
    for (title, value, binary) in [
        ("Local property value:\n", mine, b_mine),
        ("Incoming property value:\n", incoming, b_in),
    ] {
        if value.is_empty() {
            continue;
        }
        out.extend_from_slice(title.as_bytes());
        if binary {
            out.extend_from_slice(b"Cannot display: property value is binary data\n");
        } else {
            out.extend_from_slice(value);
        }
        out.push(b'\n');
    }
    out
}

fn lookup<'a>(props: Option<&'a [PropPair]>, name: &str) -> Option<&'a [u8]> {
    props?
        .iter()
        .find(|(n, _)| n == name.as_bytes())
        .map(|(_, v)| v.as_slice())
}

/// Contents of the `.prej` file for a property conflict (`svn_wc__create_prejfile` without the
/// legacy skel). `pristine_props` are the node's pristine properties, used as the reference
/// value for a merge; for an update or switch the reference is the incoming "changed from"
/// value recorded in the conflict.
pub fn prej_text(
    operation: &Operation,
    conflict: &PropConflict,
    pristine_props: Option<&[PropPair]>,
) -> Vec<u8> {
    let mut names: Vec<&String> = conflict.names.iter().collect();
    names.sort();
    let mut out = Vec::new();
    for name in names {
        let incoming_base = lookup(conflict.old_props.as_deref(), name);
        let original = if *operation == Operation::Merge {
            lookup(pristine_props, name)
        } else {
            incoming_base
        };
        let values = PropValues {
            original,
            mine: lookup(conflict.mine_props.as_deref(), name),
            incoming: lookup(conflict.their_props.as_deref(), name),
            incoming_base,
        };
        out.extend_from_slice(&conflict_description(name, &values));
    }
    out
}

#[cfg(test)]
mod tests;
