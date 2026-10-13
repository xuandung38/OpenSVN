//! Text that the app shows to the user and that came from somewhere it does not control (the
//! WebView, a file name on disk, a working copy name, a server).
//!
//! Bidirectional controls (U+202E "right-to-left override" and friends), zero-width characters,
//! line separators and other controls can reorder or hide text, so `gpj.exe` can look like
//! `exe.jpg` and an argument can push the dangerous part out of view. Two policies:
//!
//! * **reject** ([`has_deceptive`]) where the user is asked to approve the text (the program
//!   confirmation dialog) or where the text came from the WebView as a dialog title;
//! * **escape** ([`escape_for_display`]) where the text is real data the user must still see (a
//!   picked file name, a working copy name in a notification): every deceptive character is
//!   replaced with a visible `\u{XXXX}` marker.

/// Characters that could hide or reorder displayed text: controls (including newline, tab, DEL
/// and C1), line and paragraph separators, every format character of Unicode category Cf (soft
/// hyphen, Arabic number signs U+0600..U+0605 and end of ayah U+06DD / U+08E2, Syriac abbreviation
/// mark, Arabic letter mark, Mongolian vowel separator, zero-width characters, bidirectional
/// embeddings, overrides and isolates, invisible operators, interlinear annotation, the BOM,
/// Kaithi number signs, Egyptian hieroglyph format controls, shorthand format controls, musical
/// symbol beam and phrase controls U+1D173..U+1D17A, tag characters U+E0000..U+E007F), plus the
/// blank-looking combining grapheme joiner, Khmer inherent vowels and Hangul fillers.
///
/// This is the one rule for the whole app: program confirmation, new folder names, picked file
/// names and notifications all use it.
pub fn is_deceptive(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{034F}'
                | '\u{0600}'..='\u{0605}'
                | '\u{061C}'
                | '\u{06DD}'
                | '\u{070F}'
                | '\u{08E2}'
                | '\u{115F}'
                | '\u{1160}'
                | '\u{17B4}'
                | '\u{17B5}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{3164}'
                | '\u{FEFF}'
                | '\u{FFA0}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{110BD}'
                | '\u{110CD}'
                | '\u{13430}'..='\u{1343F}'
                | '\u{1BCA0}'..='\u{1BCA3}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0000}'..='\u{E007F}'
        )
}

/// Whether `text` contains any [`is_deceptive`] character.
pub fn has_deceptive(text: &str) -> bool {
    text.chars().any(is_deceptive)
}

/// `text` with every [`is_deceptive`] character written as a visible `\u{XXXX}` marker, cut to at
/// most `max_chars` characters of the original (an ellipsis marks the cut).
pub fn escape_for_display(text: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, c) in text.chars().enumerate() {
        if i == max_chars {
            out.push('\u{2026}');
            break;
        }
        if is_deceptive(c) {
            out.push_str(&format!("\\u{{{:04X}}}", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn bidi_zero_width_separators_and_controls_are_deceptive() {
        for c in [
            '\u{202A}',
            '\u{202B}',
            '\u{202C}',
            '\u{202D}',
            '\u{202E}', // embeddings, overrides
            '\u{2066}',
            '\u{2067}',
            '\u{2068}',
            '\u{2069}', // isolates
            '\u{200E}',
            '\u{200F}',
            '\u{061C}', // marks
            '\u{200B}',
            '\u{200C}',
            '\u{200D}',
            '\u{2060}',
            '\u{FEFF}', // zero width
            '\u{2028}',
            '\u{2029}', // separators
            '\u{00AD}',
            '\u{180E}',
            '\u{3164}', // invisible
            '\u{E0001}',
            '\u{E0041}',
            '\u{E007F}',
            '\u{1BCA0}', // tags, shorthand format
            '\u{0600}',
            '\u{0605}',
            '\u{06DD}',
            '\u{070F}',
            '\u{08E2}', // Arabic and Syriac prepended format characters
            '\u{1D173}',
            '\u{1D17A}', // musical symbol format controls
            '\u{115F}',
            '\u{1160}',
            '\u{FFA0}', // Hangul fillers
            '\u{110BD}',
            '\u{13430}',
            '\n',
            '\r',
            '\t',
            '\u{0}',
            '\u{7F}',
            '\u{85}',
            '\u{9B}', // controls
        ] {
            assert!(is_deceptive(c), "U+{:04X}", u32::from(c));
        }
        for c in [
            'a', 'Z', ' ', '-', '.', '/', '\\', 'é', 'ß', 'д', 'ש', 'ع', '中', '😀',
        ] {
            assert!(!is_deceptive(c), "{c}");
        }
    }

    #[test]
    fn escaping_makes_overrides_visible_and_keeps_ordinary_text() {
        assert_eq!(escape_for_display("report.txt", 100), "report.txt");
        assert_eq!(
            escape_for_display("photo\u{202E}gpj.exe", 100),
            "photo\\u{202E}gpj.exe"
        );
        assert_eq!(
            escape_for_display("a\nb\u{2028}c", 100),
            "a\\u{000A}b\\u{2028}c"
        );
        assert_eq!(escape_for_display("Проект שלום", 100), "Проект שלום");
        assert!(!has_deceptive(&escape_for_display(
            "x\u{2067}y\u{2069}",
            100
        )));
    }

    #[test]
    fn escaping_truncates_by_characters_not_bytes() {
        assert_eq!(escape_for_display("ééééé", 3), "ééé\u{2026}");
        assert_eq!(escape_for_display("abc", 3), "abc");
    }
}
