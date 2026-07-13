//! 引擎内部共享类型
//!
//! 集中放引擎模块间共享的类型别名、状态类型。
//! 当前为骨架阶段，随引擎各子系统（ReAct 循环、流式、工具执行等）逐步填充而扩展。

/// 会话 ID
///
/// 当前直接用 `String`，与 session crate 的 `Session.id` 类型一致。
/// 采用简单别名而非 newtype，避免与持久化层频繁转换；
/// 后续若类型安全需求增强，可提升为 newtype。
pub type SessionId = String;
