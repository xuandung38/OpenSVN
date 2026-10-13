//! Error classification of I/O failures.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::{Error, ErrorKind};

use svn_types::{ErrorCode, SvnError};

#[test]
fn io_errors_are_classified() {
    let cases = [
        (ErrorKind::NotFound, ErrorCode::PathNotFound),
        (ErrorKind::PermissionDenied, ErrorCode::PermissionDenied),
        (ErrorKind::StorageFull, ErrorCode::DiskFull),
        (ErrorKind::TimedOut, ErrorCode::Timeout),
        (ErrorKind::ResourceBusy, ErrorCode::FileInUse),
        (ErrorKind::InvalidData, ErrorCode::Internal),
    ];
    for (kind, code) in cases {
        assert_eq!(
            SvnError::from_io("ctx", &Error::from(kind)).code,
            code,
            "{kind:?}"
        );
    }
}

#[cfg(windows)]
#[test]
fn windows_sharing_and_lock_violations_are_file_in_use() {
    for raw in [32, 33] {
        assert_eq!(
            SvnError::from_io("ctx", &Error::from_raw_os_error(raw)).code,
            ErrorCode::FileInUse
        );
    }
}
