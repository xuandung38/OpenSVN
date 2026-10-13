//! Removes secrets and (optionally) personal data from diagnostic text.
//!
//! This is a second line of defense: log sinks already redact when writing; the bundle is
//! scrubbed again at export because the user is about to share it.

/// Header or field names whose value is always secret. Compared case-insensitively.
const SECRET_NAMES: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "x-auth-token",
    "password",
    "passwd",
    "token",
    "secret",
    "lock-token",
    "opaquelocktoken",
];

const MASK: &str = "[redacted]";

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Replaces `userinfo` in `scheme://userinfo@host` with the mask.
fn mask_userinfo(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| {
                c == '/' || c == '?' || c == '#' || c.is_whitespace() || c == '"' || c == '\''
            })
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str(MASK);
                out.push('@');
                out.push_str(&authority[at + 1..]);
            }
            None => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Masks the value that follows a secret name (`name: value`, `name=value`, `"name":"value"`).
fn mask_named_values(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    let bytes = line.as_bytes();
    'outer: while i < line.len() {
        if !line.is_char_boundary(i) {
            i += 1;
            continue;
        }
        for name in SECRET_NAMES {
            if lower[i..].starts_with(name)
                && (i == 0 || !line[..i].chars().next_back().is_some_and(is_name_char))
            {
                let after = i + name.len();
                // optional closing quote, spaces, then a separator
                let mut j = after;
                if line[j..].starts_with('"') || line[j..].starts_with('\'') {
                    j += 1;
                }
                while line[j..].starts_with(' ') {
                    j += 1;
                }
                if line[j..].starts_with(':') || line[j..].starts_with('=') {
                    j += 1;
                    while line[j..].starts_with(' ') {
                        j += 1;
                    }
                    let quoted = line[j..].starts_with('"') || line[j..].starts_with('\'');
                    let start = if quoted { j + 1 } else { j };
                    let end = if quoted {
                        let q = bytes[j] as char;
                        line[start..].find(q).map_or(line.len(), |k| start + k)
                    } else {
                        // unquoted values run to the end of the line (headers) or next delimiter
                        line[start..]
                            .find([',', ';', '&'])
                            .map_or(line.len(), |k| start + k)
                    };
                    // `Authorization: Basic abc` and `Bearer x` carry a scheme word then the secret:
                    // the whole remaining value is masked.
                    out.push_str(&line[i..start]);
                    out.push_str(MASK);
                    i = end;
                    continue 'outer;
                }
            }
        }
        let ch = line[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Removes URL credentials and the values of secret headers and fields from `text`.
pub fn redact_secrets(text: &str) -> String {
    text.lines()
        .map(|l| mask_named_values(&mask_userinfo(l)))
        .collect::<Vec<_>>()
        .join("\n")
        + if text.ends_with('\n') { "\n" } else { "" }
}

/// What to hide besides secrets.
#[derive(Debug, Clone, Default)]
pub struct PrivacyFilter {
    /// Strings to replace (home folder, working copy roots) with their placeholder.
    pub path_replacements: Vec<(String, String)>,
    /// Hide commit messages (lines with a `message` field).
    pub hide_commit_messages: bool,
}

impl PrivacyFilter {
    /// Applies the filter to `text`.
    pub fn apply(&self, text: &str) -> String {
        let mut out = text.to_owned();
        // Longest paths first so a working copy inside the home folder gets its own placeholder.
        let mut reps = self.path_replacements.clone();
        reps.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
        for (from, to) in reps {
            if !from.is_empty() {
                out = out.replace(&from, &to);
            }
        }
        if self.hide_commit_messages {
            out = out
                .lines()
                .map(|l| {
                    let lower = l.to_ascii_lowercase();
                    match lower.find("message") {
                        Some(i) if l[i..].contains('=') || l[i..].contains(':') => {
                            let cut = i + l[i..].find(['=', ':']).unwrap_or(0) + 1;
                            format!("{} {MASK}", &l[..cut])
                        }
                        _ => l.to_owned(),
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
                + if text.ends_with('\n') { "\n" } else { "" };
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn headers_urls_and_fields_are_masked() {
        let text = "GET https://alice:hunter2@svn.test.local:8443/repo HTTP/1.1\n\
                    Authorization: Basic YWxpY2U6aHVudGVyMg==\n\
                    Cookie: session=abc123; theme=dark\n\
                    {\"password\":\"p@ss w0rd\",\"user\":\"alice\"}\n\
                    token=abcdef&x=1\n\
                    plain line stays\n";
        let out = redact_secrets(text);
        for secret in [
            "hunter2",
            "YWxpY2U6aHVudGVyMg==",
            "abc123",
            "p@ss w0rd",
            "abcdef",
        ] {
            assert!(!out.contains(secret), "{secret} leaked: {out}");
        }
        assert!(out.contains("svn.test.local:8443/repo"));
        assert!(out.contains("plain line stays"));
        assert!(out.contains("\"user\":\"alice\""));
    }

    #[test]
    fn names_inside_longer_words_are_left_alone() {
        let out = redact_secrets("tokenizer: fast\nmy-cookie-jar=1\n");
        assert!(out.contains("tokenizer: fast"));
    }

    #[test]
    fn privacy_filter_replaces_paths_and_hides_messages() {
        let f = PrivacyFilter {
            path_replacements: vec![
                ("/Users/alice".into(), "~".into()),
                ("/Users/alice/src/proj".into(), "<wc-1>".into()),
            ],
            hide_commit_messages: true,
        };
        let out = f.apply("open /Users/alice/src/proj/a.rs from /Users/alice/x\ncommit message=\"fix secret thing\" rev=3\n");
        assert!(out.contains("<wc-1>/a.rs") && out.contains("~/x"));
        assert!(!out.contains("secret thing") && !out.contains("alice"));
    }

    proptest! {
        #[test]
        fn random_secrets_never_survive(secret in "[A-Za-z0-9]{8,24}") {
            let text = format!(
                "https://user:{secret}@host/p\nAuthorization: Bearer {secret}\nCookie: sid={secret}\n{{\"password\":\"{secret}\"}}\nlock-token: {secret}\n"
            );
            let out = redact_secrets(&text);
            prop_assert!(!out.contains(&secret), "{}", out);
        }
    }
}
