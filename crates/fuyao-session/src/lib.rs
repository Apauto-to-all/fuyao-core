//! fuyao-session 会话管理（重建中）
//!
//! 当前阶段：持久化地基（[`SessionStore`]：Session + Message 的 SQLite CRUD）
//! + 上下文压缩（[`compressor`]：阈值检测 + 摘要生成 + 边界落库）
//! + 费用计算（[`cost`] 模块：单条消息算费 / 填字段，全部 Decimal 精确）
//! + 标题生成（[`title_generator`]：首轮后异步生成，跨 Provider fast 优先，内部自建 Provider）。
//!
//! session 总计（total_* / total_cost）的累积由 [`SessionStore::insert_message`] 事务内
//! SQL 原子自增完成——DB 唯一数据源，不再有内存累积逻辑。
//!
//! 模块边界：
//! - `error`：Session 错误类型
//! - `schema`：SQLite DDL + 版本管理
//! - `store`：会话存储层（连接池 + CRUD + 压缩边界 + 单字段更新写入）
//! - `compressor`：上下文压缩（触发 / 窗口 / 摘要 / 落地）
//! - `cost`：费用计算（所有 cost 运算集中在此模块）
//! - `title_generator`：标题生成（纯执行层，落库与事件发布交调用方）

mod compressor;
mod cost;
mod error;
mod schema;
mod store;
mod title_generator;

pub use compressor::{apply, generate_summary, should_compress};
pub use cost::{calculate_cost, fill_message_cost};
pub use error::SessionError;
pub use store::SessionStore;
pub use title_generator::maybe_generate_title;
