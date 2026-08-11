//! 上下文压缩模块
//!
//! 设计要点：
//! - 压缩是「主循环 pre-turn 主动触发的内部重写」——不是消息、不是输入
//! - 同步执行（不开新队列、不开新通道、不需要状态机识别 LLM 输出）
//! - 摘要 LLM 调用强制 `tools=[]`，独立于主 ReAct 流
//! - 两层切分：触发（纯函数）/ 执行（异步调 LLM）；落库由调用方直接调
//!   [`crate::SessionStore::mark_compaction`]（事务内 INSERT 边界 + UPDATE 元数据）
//!
//! 文件组织：
//! - [`trigger`]：触发层（阈值检测）
//! - [`summary`]：执行层（摘要 system prompt + 调 provider + 失败处理）
//!
//! 窗口切分算法已迁移至 [`crate::store::window`]（服务于读取侧的可见窗口拼接），
//! 本模块只管压缩写侧，不参与窗口切分。

pub mod summary;
pub mod trigger;

pub use summary::generate_summary;
pub use trigger::should_compress;
