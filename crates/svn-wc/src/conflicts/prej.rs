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
//! * The "fuzzy" ASCII rendering of non-ASCII values (`svn_utf__fuzzy_escape`) is done with
//!   NFKD decomposition and combining-mark stripping (`é` becomes `e`, verified against svn),
//!   but utf8proc's `LUMP` mapping (typographic quotes and dashes to ASCII) is not reproduced:
//!   such characters appear as `{U+2019}`.
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

/// `svn_utf__fuzzy_escape`: an ASCII-only rendering for display.
///
/// ASCII passes through; CR and CRLF become LF, tab and form feed become a space, other control
/// characters are dropped; invalid bytes become `?\XX`; other characters are NFKD-decomposed,
/// combining marks dropped, and what is still non-ASCII becomes `{U+XXXX}`.
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
        let mut chars = valid.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    out.push('\n');
                }
                '\n' => out.push('\n'),
                '\t' | '\x0c' => out.push(' '),
                c if c.is_control() => {}
                c if c.is_ascii() => out.push(c),
                c => {
                    for d in std::iter::once(c).nfkd() {
                        if canonical_combining_class(d) != 0 {
                            continue;
                        }
                        if d.is_ascii() {
                            if d.is_ascii_control() && !matches!(d, '\n') {
                                continue;
                            }
                            out.push(d);
                        } else if (d as u32) > 0xFFFF {
                            let _ = write!(out, "{{U+{:06X}}}", d as u32);
                        } else {
                            let _ = write!(out, "{{U+{:04X}}}", d as u32);
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
        let merged = merge3(
            fuzzy_escape(incoming_base).as_bytes(),
            fuzzy_escape(mine).as_bytes(),
            fuzzy_escape(incoming).as_bytes(),
            &opts,
            &CancelToken::new(),
        );
        if let Ok(result) = merged
            && !result.conflicts.is_empty()
        {
            out.extend_from_slice(&result.text);
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
