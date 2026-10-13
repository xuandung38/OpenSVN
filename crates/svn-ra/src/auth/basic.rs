//! `Authorization: Basic` header (RFC 7617, UTF-8 charset).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::HeaderValue;
use secrecy::{ExposeSecret, SecretString};
use svn_types::{ErrorCode, Result, SvnError};

/// Builds the header value, marked sensitive so HTTP stacks never print it.
pub fn basic_header(username: &str, password: &SecretString) -> Result<HeaderValue> {
    let mut plain = format!("{username}:{}", password.expose_secret());
    let encoded = STANDARD.encode(plain.as_bytes());
    // Best-effort scrub of the plaintext copy.
    plain.clear();
    let mut value = HeaderValue::from_str(&format!("Basic {encoded}")).map_err(|_| {
        SvnError::new(
            ErrorCode::Internal,
            "credentials cannot be encoded as a header",
        )
    })?;
    value.set_sensitive(true);
    Ok(value)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn encodes_utf8_credentials() {
        let v = basic_header("Aladdin", &SecretString::from("open sesame")).expect("header");
        assert_eq!(
            v.to_str().expect("ascii"),
            "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
        assert!(v.is_sensitive());
        let v = basic_header("ngườidùng", &SecretString::from("mậtkhẩu")).expect("header");
        let b64 = v.to_str().expect("ascii").trim_start_matches("Basic ");
        let raw = STANDARD.decode(b64).expect("base64");
        assert_eq!(String::from_utf8(raw).expect("utf8"), "ngườidùng:mậtkhẩu");
    }
}
