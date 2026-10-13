//! Working copy registry (3.1) and status / local changes (3.2).

use opensvn_ipc::{
    DiffGetRequest, FileDiff, FileStatus, IgnoreAddRequest, OperationHandle, StatusGetRequest,
    WcId, WorkingCopy,
};

use super::stub;

stub!(wc_list() -> Vec<WorkingCopy>);
stub!(wc_add_existing(path: String) -> WorkingCopy);
stub!(wc_remove(wc_id: WcId) -> ());
stub!(wc_rename(wc_id: WcId, name: String) -> ());
stub!(wc_open(wc_id: WcId) -> WorkingCopy);
stub!(wc_close(wc_id: WcId) -> ());

stub!(status_get(request: StatusGetRequest) -> Vec<FileStatus>);
stub!(status_refresh(wc_id: WcId, paths: Option<Vec<String>>) -> OperationHandle);
stub!(status_remote(wc_id: WcId) -> OperationHandle);
stub!(diff_get(request: DiffGetRequest) -> FileDiff);
stub!(changelist_set(wc_id: WcId, paths: Vec<String>, name: Option<String>) -> ());
stub!(ignore_add(request: IgnoreAddRequest) -> ());
stub!(mark_move(wc_id: WcId, from: String, to: String) -> ());
