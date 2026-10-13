//! Text helpers shared by the engines: percent decoding, text sides and line matching.

use opensvn_ipc::{Eol, TextSide};

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
}
