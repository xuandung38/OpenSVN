//! Parsing of `WWW-Authenticate` challenges.

use reqwest::header::{HeaderMap, WWW_AUTHENTICATE};
use svn_ra_api::AuthMethod;

/// Authentication schemes offered by a 401 response.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Challenges {
    /// Realm of the `Basic` challenge, when offered.
    pub basic_realm: Option<String>,
    /// `Negotiate` (SPNEGO) is offered.
    pub negotiate: bool,
    /// Other schemes (`NTLM`, `Digest`, ...), lower-cased; not supported.
    pub other: Vec<String>,
}

impl Challenges {
    /// Parses every `WWW-Authenticate` header of a response.
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let mut out = Self::default();
        for value in headers.get_all(WWW_AUTHENTICATE) {
            if let Ok(text) = value.to_str() {
                out.parse_value(text);
            }
        }
        out
    }

    /// Methods the client could use, in preference order.
    pub fn offered_methods(&self) -> Vec<AuthMethod> {
        let mut v = Vec::new();
        if self.negotiate {
            v.push(AuthMethod::Negotiate);
        }
        if self.basic_realm.is_some() {
            v.push(AuthMethod::Basic);
        }
        v
    }

    /// Human-readable list of the offered schemes for error messages.
    pub fn describe(&self) -> String {
        let mut names: Vec<&str> = Vec::new();
        if self.negotiate {
            names.push("Negotiate");
        }
        if self.basic_realm.is_some() {
            names.push("Basic");
        }
        names.extend(self.other.iter().map(String::as_str));
        if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(", ")
        }
    }

    fn parse_value(&mut self, text: &str) {
        for part in split_outside_quotes(text) {
            let part = part.trim();
            let (first, rest) = part.split_once(char::is_whitespace).unwrap_or((part, ""));
            // `key=value` fragments are parameters of the previous challenge.
            if first.contains('=') {
                continue;
            }
            match first.to_ascii_lowercase().as_str() {
                "basic" => {
                    let realm = param(rest, "realm").unwrap_or_default();
                    self.basic_realm.get_or_insert(realm);
                }
                "negotiate" => self.negotiate = true,
                "" => {}
                other => self.other.push(other.to_owned()),
            }
        }
    }
}

/// Splits on commas that are not inside a quoted string.
fn split_outside_quotes(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (i, c) in text.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ',' if !quoted => {
                parts.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

/// Extracts `name="value"` or `name=value` from a challenge parameter list.
fn param(params: &str, name: &str) -> Option<String> {
    for piece in split_outside_quotes(params) {
        let Some((k, v)) = piece.split_once('=') else {
            continue;
        };
        if k.trim().eq_ignore_ascii_case(name) {
            let v = v.trim();
            let inner = v
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(v);
            return Some(inner.replace("\\\"", "\"").replace("\\\\", "\\"));
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use reqwest::header::HeaderValue;

    use super::*;

    fn headers(values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append(
                WWW_AUTHENTICATE,
                HeaderValue::from_str(v).expect("valid header"),
            );
        }
        h
    }

    #[test]
    fn parses_basic_realm_with_comma_and_spaces() {
        let c = Challenges::from_headers(&headers(&[r#"Basic realm="VisualSVN, Server""#]));
        assert_eq!(c.basic_realm.as_deref(), Some("VisualSVN, Server"));
        assert!(!c.negotiate);
    }

    #[test]
    fn parses_multiple_headers_and_combined_value() {
        let c = Challenges::from_headers(&headers(&["Negotiate", r#"Basic realm="r""#]));
        assert!(c.negotiate);
        assert_eq!(c.basic_realm.as_deref(), Some("r"));
        assert_eq!(
            c.offered_methods(),
            vec![AuthMethod::Negotiate, AuthMethod::Basic]
        );

        let c = Challenges::from_headers(&headers(&[r#"Negotiate, Basic realm="x", NTLM"#]));
        assert!(c.negotiate);
        assert_eq!(c.basic_realm.as_deref(), Some("x"));
        assert_eq!(c.other, vec!["ntlm".to_owned()]);
    }

    #[test]
    fn missing_realm_is_empty_and_unknown_scheme_listed() {
        let c = Challenges::from_headers(&headers(&["Basic", "Digest realm=\"d\", nonce=\"n\""]));
        assert_eq!(c.basic_realm.as_deref(), Some(""));
        assert_eq!(c.other, vec!["digest".to_owned()]);
        assert_eq!(c.describe(), "Basic, digest");
    }
}
