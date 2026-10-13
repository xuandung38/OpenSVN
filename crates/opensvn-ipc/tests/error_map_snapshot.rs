use opensvn_ipc::{ALL_SVN_ERROR_CODES, AppError, ErrorCode, map_svn_error_code};

#[test]
fn engine_error_code_table_snapshot() {
    let table: String = ALL_SVN_ERROR_CODES
        .iter()
        .map(|c| {
            let (ipc, retryable, actions) = map_svn_error_code(*c);
            let ipc = serde_json::to_string(&ipc).unwrap();
            let actions = serde_json::to_string(actions).unwrap();
            format!("{c:?} -> {ipc} retryable={retryable} actions={actions}\n")
        })
        .collect();
    insta::assert_snapshot!(table);
}

#[test]
fn corrupt_needs_cleanup_and_unsafe_path_is_internal() {
    let (corrupt, _, _) = map_svn_error_code(opensvn_ipc::SvnErrorCode::Corrupt);
    let (unsafe_path, _, _) = map_svn_error_code(opensvn_ipc::SvnErrorCode::UnsafePath);
    assert_eq!(corrupt, ErrorCode::WcNeedsCleanup);
    assert_eq!(unsafe_path, ErrorCode::Internal);
}

#[test]
fn app_error_wire_shape() {
    let e = AppError::from_code(opensvn_ipc::SvnErrorCode::OutOfDate, "File is out of date")
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
        })
    );
    assert_eq!(serde_json::from_value::<AppError>(v).unwrap(), e);
}

#[test]
fn not_implemented_is_internal() {
    let e = AppError::not_implemented("wc_list");
    assert_eq!(e.code, ErrorCode::Internal);
    assert_eq!(e.message, "not implemented");
    assert!(!e.retryable && e.actions.is_none());
}
