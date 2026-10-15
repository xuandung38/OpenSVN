//! Hard caps for everything the parsers buffer.
//!
//! A hostile server must not be able to make the client allocate without bound: exceeding any
//! cap is a protocol error (`ErrorCode::ProtocolViolation`), never an out-of-memory condition.
//! Text that belongs to an element a parser does not know is not buffered at all; the XML reader
//! still bounds every tag, attribute set and nesting depth (see `XmlLimits`).

/// Size and count limits shared by the response parsers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParserLimits {
    /// Longest `D:href` or other URL-like value.
    pub max_href_bytes: usize,
    /// Longest `D:status` line.
    pub max_status_bytes: usize,
    /// Longest single text node (property value, revision property, lock comment).
    pub max_text_bytes: usize,
    /// Longest log message; raise it for repositories with huge messages.
    pub max_log_message_bytes: usize,
    /// Most child and descendant elements inside one property value.
    pub max_prop_elements: usize,
    /// Most properties in one `D:response`.
    pub max_props_per_entry: usize,
    /// Most bytes of property text in one `D:response`.
    pub max_entry_bytes: usize,
    /// Most changed paths in one log item.
    pub max_changed_paths: usize,
    /// Longest error message kept from a `D:error` body.
    pub max_error_message_bytes: usize,
    /// Largest error body inspected.
    pub max_error_body_bytes: usize,
}

impl Default for ParserLimits {
    fn default() -> Self {
        Self {
            max_href_bytes: 8 * 1024,
            max_status_bytes: 8 * 1024,
            max_text_bytes: 16 << 20,
            max_log_message_bytes: 16 << 20,
            max_prop_elements: 10_000,
            max_props_per_entry: 1024,
            max_entry_bytes: 64 << 20,
            max_changed_paths: 1_000_000,
            max_error_message_bytes: 4 * 1024,
            max_error_body_bytes: 64 * 1024,
        }
    }
}

impl ParserLimits {
    /// Errors when `len` exceeds `max`; `what` names the field in the message.
    pub(crate) fn check(len: usize, max: usize, what: &str) -> svn_types::Result<()> {
        if len > max {
            Err(crate::util::corrupt(format!(
                "{what} exceeds the limit of {max} bytes"
            )))
        } else {
            Ok(())
        }
    }
}
