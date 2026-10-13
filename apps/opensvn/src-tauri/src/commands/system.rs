//! System commands (3.5): operations, auth, accounts, settings, OS integration.

use opensvn_ipc::{
    Account, AuthResponse, OpId, OpenWith, OperationHandle, Settings, SettingsPatch, WcId,
};

use super::stub;

stub!(op_cancel(op_id: OpId) -> ());
stub!(op_list() -> Vec<OperationHandle>);
stub!(auth_respond(request_id: String, response: AuthResponse) -> ());
stub!(accounts_list() -> Vec<Account>);
stub!(accounts_sign_out(realm: String) -> ());
stub!(settings_get() -> Settings);
stub!(settings_set(patch: SettingsPatch) -> Settings);
stub!(reveal_in_os(wc_id: WcId, path: String) -> ());
stub!(open_external(wc_id: WcId, path: String, open_with: OpenWith) -> ());
// Returns the path of the created diagnostics zip.
stub!(diagnostics_export() -> String);
