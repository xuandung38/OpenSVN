//! Namespace-aware streaming XML reading and deterministic writing.

pub mod ns;
pub mod reader;
pub mod writer;

pub use ns::{Name, Ns};
pub use reader::{Attr, StartTag, XmlEvent, XmlLimits, XmlReader, read_all};
pub use writer::XmlWriter;
