#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use opensvn_ipc::{AppError, ErrorCode, LockConflict, map_svn_error_code};
use svn_types::{ErrorCode as SvnErrorCode, SvnError};

/// Exhaustive on purpose (no wildcard): adding an engine code breaks this test until mapped.
fn all_codes() -> Vec<SvnErrorCode> {
    use SvnErrorCode::*;
    let mut v = vec![];
    for c in [
        Network,
        Timeout,
        AuthRequired,
        NegotiateFailed,
        Forbidden,
        TlsUntrusted,
        OutOfDate,
        LockedByOther,
        LockTokenInvalid,
        WcBusy,
        WcNeedsCleanup,
        WcFormatUnsupported,
        NotAWorkingCopy,
        ConflictBlocksCommit,
        HookFailed,
        RepositoryMoved,
        PristineNotHydrated,
        WcInvalidOperation,
        PathNotFound,
        UnsafePath,
        DiskFull,
        PermissionDenied,
        FileInUse,
        Cancelled,
        CommitOutcomeUnknown,
        LimitExceeded,
        Corrupt,
        ServerUnsupported,
        Internal,
    ] {
        match c {
            Network | Timeout | AuthRequired | NegotiateFailed | Forbidden | TlsUntrusted
            | OutOfDate | LockedByOther | LockTokenInvalid | WcBusy | WcNeedsCleanup
            | WcFormatUnsupported | NotAWorkingCopy | ConflictBlocksCommit | HookFailed
            | RepositoryMoved | PristineNotHydrated | WcInvalidOperation | PathNotFound
            | UnsafePath | DiskFull | PermissionDenied | FileInUse | Cancelled
            | CommitOutcomeUnknown | LimitExceeded | Corrupt | ServerUnsupported | Internal => {
                v.push(c)
            }
        }
    }
    v
}

#[test]
fn engine_error_code_table_snapshot() {
    let table: String = all_codes()
        .iter()
        .map(|c| {
            let (ipc, retryable, actions) = map_svn_error_code(*c);
            let ipc = serde_json::to_string(&ipc).unwrap_or_default();
            let actions = serde_json::to_string(actions).unwrap_or_default();
            format!("{c:?} -> {ipc} retryable={retryable} actions={actions}\n")
        })
        .collect();
    insta::assert_snapshot!(table);
}

#[test]
fn corrupt_needs_cleanup_and_unsafe_path_is_internal() {
    let (corrupt, _, _) = map_svn_error_code(SvnErrorCode::Corrupt);
    let (unsafe_path, _, _) = map_svn_error_code(SvnErrorCode::UnsafePath);
    assert_eq!(corrupt, ErrorCode::WcNeedsCleanup);
    assert_eq!(unsafe_path, ErrorCode::Internal);
}

#[test]
fn limit_exceeded_is_final_and_offers_no_cleanup() {
    let e = AppError::from(&SvnError::new(
        SvnErrorCode::LimitExceeded,
        "the folder listing has more than 1000000 entries",
    ));
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert!(!e.retryable);
    assert_eq!(e.actions, None);
    assert_eq!(
        e.message,
        "the folder listing has more than 1000000 entries"
    );
}

#[test]
fn app_error_wire_shape() {
    let e = AppError::from_code(SvnErrorCode::OutOfDate, "File is out of date")
        .with_detail("E160028")
        .with_path("src/a.txt");
    let v = serde_json::to_value(&e).unwrap();
    assert_eq!(
        v,
        serde_json::json!({
            "code": "out-of-date",
            "message": "File is out of date",
            "detail": "E160028",
            "path": "src/a.txt",
            "retryable": false,
            "actions": ["update"],
            "lockConflicts": null,
        })
    );
    assert_eq!(serde_json::from_value::<AppError>(v).unwrap(), e);
}

#[test]
fn lock_conflicts_ride_on_the_error_only_when_known() {
    let e = AppError::from_code(SvnErrorCode::LockedByOther, "locked").with_lock_conflicts(vec![
        LockConflict {
            path: "README.md".into(),
            owner: "an".into(),
            comment: Some("editing".into()),
            created_at: Some("2026-10-14T05:00:00Z".into()),
        },
    ]);
    let v = serde_json::to_value(&e).unwrap();
    assert_eq!(v["actions"], serde_json::json!(["open-lock-dialog"]));
    assert_eq!(
        v["lockConflicts"],
        serde_json::json!([{
            "path": "README.md",
            "owner": "an",
            "comment": "editing",
            "createdAt": "2026-10-14T05:00:00Z",
        }])
    );
    assert_eq!(serde_json::from_value::<AppError>(v).unwrap(), e);
    let none =
        AppError::from_code(SvnErrorCode::LockedByOther, "locked").with_lock_conflicts(vec![]);
    assert!(none.lock_conflicts.is_none());
    // Payloads without the field still read.
    let mut v = serde_json::to_value(&none).unwrap();
    assert_eq!(v["lockConflicts"], serde_json::Value::Null);
    v.as_object_mut().unwrap().remove("lockConflicts");
    assert_eq!(serde_json::from_value::<AppError>(v).unwrap(), none);
}

#[test]
fn not_implemented_is_internal() {
    let e = AppError::not_implemented("wc_list");
    assert_eq!(e.code, ErrorCode::Internal);
    assert_eq!(e.message, "not implemented");
    assert!(!e.retryable && e.actions.is_none());
}

#[test]
fn svn_error_detail_carries_code_and_chain() {
    let child = SvnError::new(SvnErrorCode::Network, "connection reset").with_svn_code(170013);
    let mut top = SvnError::new(SvnErrorCode::OutOfDate, "commit failed").with_svn_code(160028);
    top.child = Some(Box::new(child));
    let e = AppError::from(&top);
    assert_eq!(e.code, ErrorCode::OutOfDate);
    assert_eq!(
        e.detail.as_deref(),
        Some("E160028: commit failed <- E170013: connection reset")
    );
    let plain = AppError::from(&SvnError::new(SvnErrorCode::Cancelled, "stopped"));
    assert_eq!(plain.detail, None);
}
