//! fuyao-session 会话管理（重建中）
//!
//! 当前阶段：最小持久化地基——只提供 [`SessionStore`]（Session + Message 的 SQLite CRUD）。
//!
//! 模块边界（当前）：
//! - `error`：Session 错误类型
//! - `schema`：SQLite DDL + 版本管理
//! - `store`：会话存储层（连接池 + CRUD）
//!
//! 后续按需逐层补全（均不在当前阶段）：
//! - 内存态会话管理（消息历史的内存表示 + 边界时刻落库）
//! - 与引擎内核的协作契约
//! - 运行时增强（上下文压缩、标题生成、费用统计等，各自独立、可插拔）

mod error;
mod schema;
mod store;

pub use error::SessionError;
pub use store::SessionStore;
