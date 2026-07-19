//! fuyao-session 会话管理（重建中）
//!
//! 当前阶段：持久化地基（[`SessionStore`]：Session + Message 的 SQLite CRUD）
//! + 上下文压缩（[`compressor`]：阈值检测 + 摘要生成 + 边界落库）。
//!
//! 模块边界：
//! - `error`：Session 错误类型
//! - `schema`：SQLite DDL + 版本管理
//! - `store`：会话存储层（连接池 + CRUD + 压缩边界写入）
//! - `compressor`：上下文压缩（触发 / 窗口 / 摘要 / 落地）

mod compressor;
mod error;
mod schema;
mod store;

pub use compressor::{
    apply, generate_summary, should_compress, CompressionState as CompressionRuntimeState,
};
pub use error::SessionError;
pub use store::SessionStore;
