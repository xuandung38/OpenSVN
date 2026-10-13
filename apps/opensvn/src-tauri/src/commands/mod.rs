//! Command stubs for every command in docs/system-design/03 section 3.
//! Each returns `AppError { code: internal, message: "not implemented" }` until the backend lands.

mod remote;
mod system;
mod working_copy;
mod write_ops;

pub use remote::*;
pub use system::*;
pub use working_copy::*;
pub use write_ops::*;

/// Declares a stub command: `stub!(name(arg: Type, ...) -> Ret);`
macro_rules! stub {
    ($(#[$m:meta])* $name:ident ( $($arg:ident : $ty:ty),* $(,)? ) -> $ret:ty) => {
        $(#[$m])*
        #[tauri::command]
        #[specta::specta]
        #[allow(unused_variables)]
        pub async fn $name($($arg: $ty),*) -> Result<$ret, opensvn_ipc::AppError> {
            Err(opensvn_ipc::AppError::not_implemented(stringify!($name)))
        }
    };
}
pub(crate) use stub;
