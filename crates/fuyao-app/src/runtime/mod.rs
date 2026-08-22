//! 装配产物门面：[`crate::FuyaoApp`] 三个字段类型所在
//!
//! 三者均由 [`crate::start`] 构造注入（引擎 / 共享 store / 启动路径身份），
//! start 之后可用。

mod app;
mod discovery;
mod session_manager;

pub use app::App;
pub use discovery::Discovery;
pub use session_manager::SessionManager;
