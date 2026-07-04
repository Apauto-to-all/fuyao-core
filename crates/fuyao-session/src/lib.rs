//! 会话管理模块
//!
//! SQLite 持久化、缓存、Todo 管理、上下文压缩。

pub mod compressor;
pub mod context;
pub mod cost;
pub mod error;
pub mod manager;
pub mod schema;
pub mod store;
pub mod todo_store;
pub mod utils;

pub use context::{SessionContext, register_session_hooks};
pub use cost::calculate_cost;
pub use error::SessionError;
pub use manager::{SessionManager, clear_session_manager_cache, get_session_manager};
pub use store::SQLiteStore;
pub use todo_store::TodoStore;
