//! 工具调用上下文
//!
//! 工具执行时的上下文信息，由编排层注入。
//! 不出现在工具 Schema 中，LLM 无法访问。

use crate::AgentPaths;
use crate::tool::ops::{SubagentOps, TodoStoreOps};
use std::path::Path;
use std::sync::{Arc, Weak};

use tokio::sync::mpsc::UnboundedSender;

use crate::message::OutputEvent;

/// 工具调用上下文
///
/// 工具执行时的上下文信息，由编排层注入：
/// - `session_id` / `agent_paths`：当前 session 的身份信息
/// - `tool_call_id`：触发本次执行的 LLM tool_call id（子代理类工具可据此关联生命周期事件）
/// - `subagent_ops`：派生子 session 的引擎能力弱引用（仅子代理类工具用，
///   handler upgrade 后调 [`SubagentOps`] 方法；普通工具忽略此字段）
/// - `event_forwarder`：父 session 出站通道的直送克隆（不盖 session_id 标签），
///   子代理类工具把子 session 的中间事件转发过来，让前端在父流里看到子代理实时进度
/// - `todo_store`：读写任务列表的存储能力强引用（仅 todo 工具用，
///   handler 直接调 [`TodoStoreOps`] 方法；普通工具忽略此字段）
///
/// 不出现在工具 Schema 中，LLM 无法访问。
#[derive(Clone, Default)]
pub struct ToolCallContext {
    /// 会话 ID
    pub session_id: Option<String>,

    /// Agent 三层目录身份证明
    pub agent_paths: Option<AgentPaths>,

    /// 触发本次执行的 LLM tool_call id
    ///
    /// 由编排层从 `ToolCallData.id` 透传，工具可据此关联派生事件（如子代理
    /// `ChildSession` 事件的 `tool_call_id` 字段）。
    pub tool_call_id: Option<String>,

    /// 引擎派生子 session 的能力弱引用（运行期注入）
    ///
    /// 普通工具不读此字段；子代理类工具 upgrade 后调 [`SubagentOps`] 方法
    /// 派生子任务 session 并消费其事件流。
    pub subagent_ops: Option<Weak<dyn SubagentOps>>,

    /// 父 session 出站通道的直送克隆（运行期注入）
    ///
    /// 普通工具不读此字段；子代理类工具把子 session 的中间事件（Chunk / ToolCall / 等）
    /// 转发到这里，让它们经父 session 的 forwarder 进 fan_out，前端可实时看到子代理进度。
    ///
    /// **不盖 session_id 标签直送**——子事件已自带 child session_id，
    /// 不能被父 emitter 的 stamp_session_id 覆盖。所以这里是 raw sender，
    /// 调用方用 `tx.send(ev)` 而非 `emitter.emit(ev)`。
    pub event_forwarder: Option<UnboundedSender<OutputEvent>>,

    /// 任务列表（todo）存储能力强引用（运行期注入）
    ///
    /// 普通工具不读此字段；todo 工具直接调 [`TodoStoreOps`] 方法读写
    /// 当前 session 的任务列表。
    ///
    /// 用 `Arc`（而非子代理字段的 `Weak`）——任务列表存储是 `SessionStore`
    /// 的能力，随引擎生命周期存活，没有「存储已关闭」的降级语义。
    pub todo_store: Option<Arc<dyn TodoStoreOps>>,
}

impl std::fmt::Debug for ToolCallContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCallContext")
            .field("session_id", &self.session_id)
            .field("agent_paths", &self.agent_paths)
            .field("tool_call_id", &self.tool_call_id)
            .field(
                "subagent_ops",
                &self.subagent_ops.as_ref().map(|_| "<SubagentOps>"),
            )
            .field(
                "event_forwarder",
                &self.event_forwarder.as_ref().map(|_| "<Sender>"),
            )
            .field(
                "todo_store",
                &self.todo_store.as_ref().map(|_| "<TodoStoreOps>"),
            )
            .finish()
    }
}

impl ToolCallContext {
    /// 获取 workspace 路径
    pub fn workspace(&self) -> Option<&Path> {
        self.agent_paths
            .as_ref()
            .and_then(|ap| ap.workspace.as_deref())
    }

    /// 获取 session_id（默认 "default"）
    pub fn task_id(&self) -> &str {
        self.session_id.as_deref().unwrap_or("default")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_context_default() {
        let ctx = ToolCallContext::default();
        assert!(ctx.session_id.is_none());
        assert!(ctx.agent_paths.is_none());
    }

    #[test]
    fn tool_call_context_workspace_accessor() {
        let ctx = ToolCallContext {
            agent_paths: Some(AgentPaths {
                workspace: Some("/tmp/project".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(ctx.workspace(), Some(Path::new("/tmp/project")));
    }

    #[test]
    fn tool_call_context_task_id_default() {
        let ctx = ToolCallContext::default();
        assert_eq!(ctx.task_id(), "default");
    }

    #[test]
    fn tool_call_context_task_id_from_session() {
        let ctx = ToolCallContext {
            session_id: Some("my_session".into()),
            ..Default::default()
        };
        assert_eq!(ctx.task_id(), "my_session");
    }
}
