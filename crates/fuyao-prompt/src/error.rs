//! 提示词模块错误类型
//!
//! 承载 Agent 定义解析（加载 + mode 校验）的失败语义。定义名是调用方显式
//! 提供的（`AgentConfig.definition` 必填），解析失败属于调用方输入错误，
//! 报错带足修正信息（未知名附可用列表、损坏附文件路径与原因），
//! 不做任何静默人格替换。

use fuyao_api::AgentMode;
use thiserror::Error;

/// Agent 定义解析错误
#[derive(Debug, Error)]
pub enum PromptError {
    /// 定义名不存在（四层目录与内置表均未命中）
    ///
    /// `available` 为按请求用途过滤后的可用定义名（顿号分隔），供调用方
    /// （或依据错误信息自纠的 LLM）直接修正定义名。
    #[error("未找到 Agent 定义 `{name}`，可用定义：{available}")]
    DefinitionNotFound {
        /// 请求的定义名
        name: String,
        /// 可用定义名列表（顿号分隔）
        available: String,
    },

    /// 定义名存在，但 mode 与请求用途不符（如 Subagent 定义用作会话人格）
    #[error("Agent 定义 `{name}` 的 mode 为 {mode:?}，不能用作{usage:?}")]
    ModeMismatch {
        /// 请求的定义名
        name: String,
        /// 定义声明的使用模式
        mode: AgentMode,
        /// 请求的用途方向
        usage: crate::PromptUsage,
    },

    /// 定义文件存在但损坏（读取或 frontmatter 解析失败）
    ///
    /// `cause` 为解析层的中文错误信息，含来源文件路径与具体原因。
    /// 与 [`PromptError::DefinitionNotFound`]（链上无此文件）是两种独立语义，
    /// 调用方可分别处理。
    #[error("Agent 定义 `{name}` 文件损坏：{cause}")]
    DefinitionCorrupted {
        /// 请求的定义名
        name: String,
        /// 解析失败的中文原因（含来源文件路径）
        cause: String,
    },
}
