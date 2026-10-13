//! Write operations on a working copy (3.3); all long ones return an `OperationHandle`.
//! `add`/`delete`/`move` are named `add_paths`/`delete_paths`/`move_path` (keyword and clarity).

use opensvn_ipc::{
    CheckoutRequest, CleanupRequest, CommitRequest, Depth, OperationHandle, ResolveRequest, Rev,
    RevertRequest, UpdateRequest, WcId,
};

use super::stub;

stub!(checkout(request: CheckoutRequest) -> OperationHandle);
stub!(update(request: UpdateRequest) -> OperationHandle);
stub!(switch(wc_id: WcId, url: String, rev: Option<Rev>) -> OperationHandle);
stub!(commit(request: CommitRequest) -> OperationHandle);
stub!(revert(request: RevertRequest) -> OperationHandle);
stub!(add_paths(wc_id: WcId, paths: Vec<String>) -> OperationHandle);
stub!(delete_paths(wc_id: WcId, paths: Vec<String>, keep_local: Option<bool>) -> OperationHandle);
stub!(move_path(wc_id: WcId, from: String, to: String) -> OperationHandle);
stub!(cleanup(request: CleanupRequest) -> OperationHandle);
stub!(set_depth(wc_id: WcId, path: String, depth: Depth) -> OperationHandle);
stub!(upgrade_format(wc_id: WcId, to: u8) -> OperationHandle);
stub!(resolve(request: ResolveRequest) -> ());
stub!(props_set(wc_id: WcId, path: String, name: String, value: Option<String>) -> ());
