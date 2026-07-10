//! 会话管理模块
//!
//! SQLite 持久化、缓存、Todo 管理、上下文压缩。

mod compressor;
mod context;
mod cost;
mod error;
mod manager;
mod schema;
mod store;
mod todo_store;
mod utils;

pub use context::{SessionContext, SessionPlugin};
pub use cost::calculate_cost;
pub use error::SessionError;
pub use manager::{SessionManager, clear_session_manager_cache, get_session_manager};
pub use store::SQLiteStore;
pub use todo_store::TodoStore;
