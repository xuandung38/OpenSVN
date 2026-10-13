//! Shared error constructors for the app layer.

use opensvn_ipc::{AppError, ErrorCode};

/// Error for a request the user or UI got wrong (bad value, expired grant, unsupported option).
pub fn invalid_argument(message: impl Into<String>) -> AppError {
    let mut e = AppError::internal(message);
    e.code = ErrorCode::InvalidArgument;
    e
}
