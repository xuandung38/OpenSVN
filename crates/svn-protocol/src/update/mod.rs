//! `update-report`: request builder and streaming event parser.

mod events;
mod parser;
mod request;

pub use events::{DriveChecker, Md5, Sha1, UpdateEvent};
pub use parser::{UpdateParser, parse_update};
pub use request::{UpdateMode, UpdateReportBuilder, UpdateRequestOptions};
