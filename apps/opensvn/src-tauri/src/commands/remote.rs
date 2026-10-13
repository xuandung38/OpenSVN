//! Remote operations that need no working copy (3.4).
//! `repo_cat` returns text (size-limited); `repo_save` streams to disk as an operation.

use opensvn_ipc::{
    BranchNode, CopyLocationRequest, Layout, LayoutDetection, LockRequest, LogGetRequest, LogPage,
    OperationHandle, RemoteEditRequest, RepoEntry, RepoInfo, Rev, TextSide, UnlockRequest,
};

use super::stub;

stub!(repo_info(url: String) -> RepoInfo);
stub!(repo_list(url: String, rev: Option<Rev>) -> Vec<RepoEntry>);
stub!(repo_cat(url: String, rev: Option<Rev>) -> TextSide);
stub!(repo_save(url: String, rev: Option<Rev>, save_to: String) -> OperationHandle);
stub!(log_get(request: LogGetRequest) -> LogPage);
// Result arrives as `op://finished` with a blame payload.
stub!(blame_get(url: String, rev: Option<Rev>) -> OperationHandle);
stub!(lock(request: LockRequest) -> OperationHandle);
stub!(unlock(request: UnlockRequest) -> OperationHandle);
stub!(remote_edit(request: RemoteEditRequest) -> OperationHandle);
stub!(branches_list(repo_root_url: String) -> Vec<BranchNode>);
stub!(layout_get(repo_uuid: String) -> Layout);
stub!(layout_set(repo_uuid: String, layout: Layout) -> ());
stub!(layout_detect(repo_root_url: String, url: String) -> LayoutDetection);
stub!(copy_location(request: CopyLocationRequest) -> OperationHandle);
