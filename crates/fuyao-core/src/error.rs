//! 引擎错误类型

use thiserror::Error;

/// 引擎错误
///
/// 按错误性质分两类：
/// - 同步校验错误：调用函数瞬间可判断，走函数 `Err` 返回
///   （如 `SessionNotFound`——session id 不在调度表或数据库）
/// - 异步执行错误：对话跑起来后发生，走事件流（带 session_id 标签）
///   本枚举主要用于前者；后者在事件流的 `OutputEvent::Error` 中承载
#[derive(Debug, Error)]
pub enum EngineError {
    /// 会话未找到（session id 不在调度表，或数据库无此记录）
    ///
    /// 调用方需明确：要恢复走恢复动作，编号错则修正编号。
    #[error("会话未找到: {0}")]
    SessionNotFound(String),

    /// 存储层错误
    #[error("存储错误: {0}")]
    Storage(#[from] fuyao_session::SessionError),

    /// 提供者（LLM）错误，用于装配或同步调用失败
    #[error("提供者错误: {0}")]
    Provider(String),

    /// Agent 定义解析错误（定义名不存在、mode 与用途不符，或定义文件损坏）
    ///
    /// 创建 / 恢复 / 派生会话时由 `resolve_definition` 产生，错误信息自带
    /// 修正所需上下文（未知名附可用列表、损坏附文件路径与原因）。
    #[error("{0}")]
    Prompt(#[from] fuyao_prompt::PromptError),

    /// 引擎已关闭（`Engine::shutdown` 已调用）
    ///
    /// shutdown 后所有 `send` / `recv` 调用立即返回此错误（或 None）：
    /// - `send`：返回 `Err(Shutdown)`（区分于 `SessionNotFound`，明确告知是引擎已关而非编号错）
    /// - `recv`：先 drain 残余事件，再返回 None（让消费者收完 shutdown 前最后几条产出）
    ///
    /// 这是「同步校验错误」——调用函数瞬间即可判断，符合 01 文档错误处理原则。
    #[error("引擎已关闭")]
    Shutdown,

    /// 会话停止超时：`Engine::stop_session` 在超时预算内未等到 turn 完全静默
    ///
    /// turn 可能卡在不响应中断信号的环节（如工具不响应取消、压缩 LLM 调用超长）。
    /// 调用方不得继续依赖「已静默」前提做后续 DB 操作（如数据库回退），可重试停止。
    #[error("会话停止超时（{timeout_secs}s）：session={session_id}")]
    StopTimeout {
        /// 停止目标的 session id
        session_id: String,
        /// 等待 turn 静默的超时预算（秒）
        timeout_secs: u64,
    },
}

/// 引擎错误到子 session 操作错误的结构化映射
///
/// 供 `impl SubagentOps for Engine` 转发时转换错误类型：会话缺失 / 引擎关闭 /
/// 存储 / 定义解析一一对应，其余变体归入内部错误兜底。
impl From<EngineError> for fuyao_api::SubagentError {
    fn from(e: EngineError) -> Self {
        match e {
            EngineError::SessionNotFound(id) => fuyao_api::SubagentError::SessionNotFound(id),
            EngineError::Storage(err) => fuyao_api::SubagentError::Storage(err.to_string()),
            EngineError::Prompt(err) => fuyao_api::SubagentError::Prompt(err.to_string()),
            EngineError::Provider(msg) => fuyao_api::SubagentError::Internal(msg),
            EngineError::Shutdown => fuyao_api::SubagentError::Shutdown,
            EngineError::StopTimeout {
                session_id,
                timeout_secs,
            } => fuyao_api::SubagentError::Internal(format!(
                "会话停止超时（{timeout_secs}s）：session={session_id}"
            )),
        }
    }
}
