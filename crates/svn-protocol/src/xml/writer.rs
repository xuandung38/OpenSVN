//! Minimal deterministic XML writer for request bodies.

use bytes::Bytes;
use quick_xml::escape::escape;
use svn_types::{ErrorCode, Result, SvnError};

/// Builds an XML document in memory. Output is byte-stable (no pretty printing), which keeps
/// snapshot tests meaningful. Characters that XML 1.0 cannot carry are reported by [`finish`].
///
/// [`finish`]: XmlWriter::finish
#[derive(Debug, Default)]
pub struct XmlWriter {
    out: String,
    open: Vec<String>,
    error: Option<SvnError>,
}

fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
}

impl XmlWriter {
    /// Creates an empty writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Writes the XML declaration used by every Subversion request body.
    pub fn declaration(&mut self) -> &mut Self {
        self.out
            .push_str(r#"<?xml version="1.0" encoding="utf-8"?>"#);
        self
    }

    fn check(&mut self, s: &str) {
        if self.error.is_none() && !s.chars().all(is_xml_char) {
            self.error = Some(SvnError::new(
                ErrorCode::Internal,
                "value contains characters that cannot be represented in XML",
            ));
        }
    }

    fn open_tag(&mut self, name: &str, attrs: &[(&str, &str)], close: &str) {
        self.out.push('<');
        self.out.push_str(name);
        for (k, v) in attrs {
            self.check(v);
            self.out.push(' ');
            self.out.push_str(k);
            self.out.push_str("=\"");
            self.out.push_str(&escape(*v));
            self.out.push('"');
        }
        self.out.push_str(close);
    }

    /// Opens an element.
    pub fn start(&mut self, name: &str, attrs: &[(&str, &str)]) -> &mut Self {
        self.open_tag(name, attrs, ">");
        self.open.push(name.to_owned());
        self
    }

    /// Closes the innermost open element.
    pub fn end(&mut self) -> &mut Self {
        if let Some(name) = self.open.pop() {
            self.out.push_str("</");
            self.out.push_str(&name);
            self.out.push('>');
        } else if self.error.is_none() {
            self.error = Some(SvnError::new(
                ErrorCode::Internal,
                "XmlWriter: end without start",
            ));
        }
        self
    }

    /// Writes a self-closing element.
    pub fn empty(&mut self, name: &str, attrs: &[(&str, &str)]) -> &mut Self {
        self.open_tag(name, attrs, "/>");
        self
    }

    /// Writes escaped character data.
    pub fn text(&mut self, text: &str) -> &mut Self {
        self.check(text);
        self.out.push_str(&escape(text));
        self
    }

    /// Writes `<name attrs>text</name>`.
    pub fn element(&mut self, name: &str, attrs: &[(&str, &str)], text: &str) -> &mut Self {
        self.start(name, attrs).text(text).end()
    }

    /// Returns the document, or the first problem recorded while writing.
    pub fn finish(self) -> Result<Bytes> {
        if let Some(e) = self.error {
            return Err(e);
        }
        if !self.open.is_empty() {
            return Err(SvnError::new(
                ErrorCode::Internal,
                "XmlWriter: unclosed element",
            ));
        }
        Ok(Bytes::from(self.out.into_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_text_and_attributes() {
        let mut w = XmlWriter::new();
        w.start("a", &[("k", "x\"<y")]).text("1 < 2 & 3").end();
        assert_eq!(
            w.finish().unwrap(),
            r#"<a k="x&quot;&lt;y">1 &lt; 2 &amp; 3</a>"#
        );
    }

    #[test]
    fn rejects_control_characters() {
        let mut w = XmlWriter::new();
        w.element("a", &[], "bad\u{1}");
        assert!(w.finish().is_err());
    }

    #[test]
    fn unbalanced_is_an_error() {
        let mut w = XmlWriter::new();
        w.start("a", &[]);
        assert!(w.finish().is_err());
    }
}
