//! `log-report`: request builder and streaming parser.

mod parser;
mod request;

pub use parser::{LogItem, LogParser, parse_log};
pub use request::{LogRequest, LogRevprops, log_request};
