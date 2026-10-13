//! Panic containment at the command boundary: a bug in a command or in the engine becomes an
//! `internal` error for that call, and the app keeps running.

use std::any::Any;
use std::future::Future;
use std::panic::AssertUnwindSafe;

use futures_util::FutureExt;
use opensvn_ipc::AppError;

/// Converts a panic payload to the error returned to the UI. The payload text goes to the log and
/// to `detail` (technical field); the user-facing message stays generic.
pub fn panic_to_error(payload: &(dyn Any + Send)) -> AppError {
    let text = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned());
    tracing::error!(panic = %text, "panic contained at the command boundary");
    AppError::internal("Internal error. The operation was stopped; the app keeps running.")
        .with_detail(format!("panic: {text}"))
}

/// Runs `fut`, turning a panic into an `internal` [`AppError`].
pub async fn guard<T>(fut: impl Future<Output = Result<T, AppError>>) -> Result<T, AppError> {
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(result) => result,
        Err(payload) => Err(panic_to_error(payload.as_ref())),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use opensvn_ipc::ErrorCode;

    #[tokio::test]
    async fn panic_becomes_internal_error() {
        let err = guard(async {
            if true {
                panic!("boom");
            }
            Ok::<(), AppError>(())
        })
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Internal);
        assert_eq!(err.detail.as_deref(), Some("panic: boom"));
        // the runtime is still usable afterwards
        assert_eq!(guard(async { Ok::<_, AppError>(7) }).await.unwrap(), 7);
    }

    #[tokio::test]
    async fn formatted_panic_payload_is_captured() {
        let err = guard(async {
            let n = 3;
            if n == 3 {
                panic!("bad {n}");
            }
            Ok::<(), AppError>(())
        })
        .await
        .unwrap_err();
        assert_eq!(err.detail.as_deref(), Some("panic: bad 3"));
    }

    #[tokio::test]
    async fn ok_and_err_pass_through() {
        assert!(
            guard(async { Err::<(), _>(AppError::internal("x")) })
                .await
                .is_err()
        );
    }
}
