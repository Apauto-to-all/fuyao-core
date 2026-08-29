//! 装配产物门面：[`crate::FuyaoApp`] 两个字段类型所在
//!
//! 二者均由 [`crate::start`] 构造注入（引擎 / 共享 store），
//! start 之后可用。

mod app;
mod session_manager;

pub use app::App;
pub use session_manager::SessionManager;
