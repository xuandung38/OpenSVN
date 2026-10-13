//! Text helpers shared by the engines: percent decoding, text sides and line matching.

use opensvn_ipc::{Eol, FileDiff, TextSide};
use svn_types::{ErrorCode, SvnError};

use crate::engine::context::OpContext;
use crate::engine::facade::DiffOptions;

/// Decodes `%XX` escapes (invalid escapes stay literal).
pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Detects the line ending style of `text`.
pub(crate) fn detect_eol(text: &str) -> Eol {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count();
    match (crlf, lf - crlf) {
        (0, _) => Eol::Lf,
        (_, 0) => Eol::Crlf,
        _ => Eol::Mixed,
    }
}

/// Whether `data` should be treated as binary (NUL byte or invalid UTF-8).
pub(crate) fn is_binary(data: &[u8]) -> bool {
    data.iter().take(8000).any(|b| *b == 0) || std::str::from_utf8(data).is_err()
}

/// Text side for UTF-8 content.
pub(crate) fn text_side(data: &[u8], label: String) -> TextSide {
    let content = String::from_utf8_lossy(data).into_owned();
    TextSide {
        eol: detect_eol(&content),
        content,
        encoding: "UTF-8".to_owned(),
        label,
    }
}

/// For each line of `new`, the index of the matching line in `old` (longest common subsequence).
///
/// Quadratic in time and memory: only for the small in-memory repositories of the fake engine.
/// The real engine attributes lines with `svn-diff` under a cost budget instead.
pub(crate) fn match_lines(old: &[&str], new: &[&str]) -> Vec<Option<usize>> {
    let (n, m) = (old.len(), new.len());
    let mut table = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if old[i] == new[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let mut out = vec![None; m];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old[i] == new[j] {
            out[j] = Some(i);
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// True when diffing `old` and `new` for display would exceed `svn_diff::DISPLAY_MAX_COST`, the
/// budget the real engine applies before giving up with `TooComplex`.
///
/// # Errors
/// `Cancelled` when `ctx` is cancelled while diffing.
pub(crate) fn exceeds_display_budget(
    old: &[u8],
    new: &[u8],
    ignore_whitespace: bool,
    ctx: &OpContext,
) -> Result<bool, SvnError> {
    let opts = svn_diff::DisplayOptions {
        diff: svn_diff::DiffOptions {
            ignore_space: if ignore_whitespace {
                svn_diff::IgnoreSpace::All
            } else {
                svn_diff::IgnoreSpace::None
            },
            ignore_eol_style: false,
            max_cost: Some(svn_diff::DISPLAY_MAX_COST),
        },
        context: 0,
    };
    match svn_diff::diff(old, new, &opts, &ctx.cancel) {
        Ok(_) => Ok(false),
        Err(svn_diff::DiffError::TooComplex) => Ok(true),
        Err(e) => Err(SvnError::new(ErrorCode::Cancelled, e.to_string())),
    }
}

/// One side of a diff: the raw bytes and the label shown above them.
pub(crate) type DiffSide = Option<(Vec<u8>, String)>;

/// Builds the [`FileDiff`] both engines return for two file sides (`None` when the file does not
/// exist on that side). Binary content is reported without sides. Unless `opts.full_text` is set,
/// a diff over the `svn_diff` display budget is flagged `too_complex` and sent without sides, so
/// the WebView never receives text it would refuse to diff.
///
/// # Errors
/// `Cancelled` when `ctx` is cancelled while diffing.
pub(crate) fn file_diff(
    path: &str,
    left: DiffSide,
    right: DiffSide,
    opts: DiffOptions,
    ctx: &OpContext,
) -> Result<FileDiff, SvnError> {
    let binary = [&left, &right]
        .iter()
        .any(|s| s.as_ref().is_some_and(|(d, _)| is_binary(d)));
    let too_complex = !binary
        && !opts.full_text
        && exceeds_display_budget(
            left.as_ref().map_or(&[][..], |(d, _)| d),
            right.as_ref().map_or(&[][..], |(d, _)| d),
            opts.ignore_whitespace,
            ctx,
        )?;
    let keep_sides = !binary && !too_complex;
    let side = |s: DiffSide| s.map(|(d, label)| text_side(&d, label));
    Ok(FileDiff {
        path: path.to_owned(),
        binary,
        mime: binary.then(|| "application/octet-stream".to_owned()),
        left: if keep_sides { side(left) } else { None },
        right: if keep_sides { side(right) } else { None },
        hunks: None,
        prop_changes: Vec::new(),
        image: None,
        too_large: None,
        too_complex: too_complex.then_some(true),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn decodes_escapes() {
        assert_eq!(percent_decode("a%20b%2Fc%zz"), "a b/c%zz");
    }

    #[test]
    fn eol_detection() {
        assert_eq!(detect_eol("a\nb\n"), Eol::Lf);
        assert_eq!(detect_eol("a\r\nb\r\n"), Eol::Crlf);
        assert_eq!(detect_eol("a\r\nb\n"), Eol::Mixed);
        assert_eq!(detect_eol("plain"), Eol::Lf);
    }

    #[test]
    fn lcs_matches_unchanged_lines() {
        let m = match_lines(&["a", "b", "c"], &["a", "x", "c", "d"]);
        assert_eq!(m, vec![Some(0), None, Some(2), None]);
    }

    fn lines(rev: bool) -> Vec<u8> {
        let mut v: Vec<usize> = (0..20_000).collect();
        if rev {
            v.reverse();
        }
        v.iter()
            .flat_map(|i| format!("line {i}\n").into_bytes())
            .collect()
    }

    type Side = Option<(Vec<u8>, String)>;

    fn sides(l: Vec<u8>, r: Vec<u8>) -> (Side, Side) {
        (Some((l, "l".into())), Some((r, "r".into())))
    }

    #[test]
    fn a_diff_over_the_display_budget_is_flagged_and_sent_without_sides() {
        let (l, r) = sides(lines(false), lines(true));
        let ctx = OpContext::detached();
        let d = file_diff("big.txt", l, r, DiffOptions::default(), &ctx).unwrap();
        assert_eq!(d.too_complex, Some(true));
        assert!(d.left.is_none() && d.right.is_none() && !d.binary);
    }

    #[test]
    fn full_text_skips_the_budget_for_the_external_tool() {
        let (l, r) = sides(lines(false), lines(true));
        let opts = DiffOptions {
            full_text: true,
            ..DiffOptions::default()
        };
        let d = file_diff("big.txt", l, r, opts, &OpContext::detached()).unwrap();
        assert_eq!(d.too_complex, None);
        assert!(d.left.unwrap().content.starts_with("line 0\n"));
        assert!(d.right.unwrap().content.starts_with("line 19999\n"));
    }

    #[test]
    fn an_ordinary_edit_keeps_both_sides() {
        let (l, r) = sides(b"a\nb\nc\n".to_vec(), b"a\nB\nc\n".to_vec());
        let d = file_diff(
            "s.txt",
            l,
            r,
            DiffOptions::default(),
            &OpContext::detached(),
        )
        .unwrap();
        assert_eq!(d.too_complex, None);
        assert_eq!(d.right.unwrap().content, "a\nB\nc\n");
    }
}
