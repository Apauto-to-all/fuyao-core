//! 上下文压缩模块
//!
//! 设计要点（对齐 1.4/1.5 决策）：
//! - 压缩是「主循环 pre-turn 主动触发的内部重写」——不是消息、不是输入
//! - 同步执行（不开新队列、不开新通道、不需要状态机识别 LLM 输出）
//! - 摘要 LLM 调用强制 `tools=[]`，独立于主 ReAct 流（三项目共识）
//! - 三层切分：触发（纯函数）/ 执行（异步调 LLM）/ 落地（调 store）
//!
//! 文件组织：
//! - [`trigger`]：触发层（阈值检测）
//! - [`window`]：窗口算法（保留 tail 的 token 预算切分 + 整 turn 完整性，读取侧可见窗口拼接用）
//! - [`summary`]：执行层（构造 prompt + 调 provider + 失败处理）
//! - [`prompt`]：摘要 system prompt + previous-summary 注入模板
//! - [`apply`]：落地层（调 store.mark_compaction）

pub mod apply;
pub mod prompt;
pub mod summary;
pub mod trigger;
pub mod window;

pub use apply::apply;
pub use summary::generate_summary;
pub use trigger::should_compress;
