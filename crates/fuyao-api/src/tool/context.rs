//! 工具调用上下文
//!
//! 工具执行时的上下文信息，由编排层注入。
//! 不出现在工具 Schema 中，LLM 无法访问。

use crate::AgentPaths;
use crate::tool::ops::SubagentOps;
use std::path::{Path, PathBuf};
use std::sync::Weak;

/// 工具调用上下文
///
/// 工具执行时的上下文信息，由编排层注入：
/// - `session_id` / `agent_paths`：当前 session 的身份信息
/// - `subagent_ops`：派生子 session 的引擎能力弱引用（仅子代理类工具用，
///   handler upgrade 后调 [`SubagentOps`] 方法；普通工具忽略此字段）
///
/// 不出现在工具 Schema 中，LLM 无法访问。
#[derive(Clone, Default)]
pub struct ToolCallContext {
    /// 会话 ID
    pub session_id: Option<String>,

    /// Agent 三层目录身份证明
    pub agent_paths: Option<AgentPaths>,

    /// 引擎派生子 session 的能力弱引用（运行期注入）
    ///
    /// 普通工具不读此字段；子代理类工具 upgrade 后调 [`SubagentOps`] 方法
    /// 派生子任务 session 并消费其事件流。
    pub subagent_ops: Option<Weak<dyn SubagentOps>>,
}

impl std::fmt::Debug for ToolCallContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCallContext")
            .field("session_id", &self.session_id)
            .field("agent_paths", &self.agent_paths)
            .field(
                "subagent_ops",
                &self.subagent_ops.as_ref().map(|_| "<SubagentOps>"),
            )
            .finish()
    }
}

impl ToolCallContext {
    /// 从工具 args JSON 中提取上下文
    ///
    /// 提取 `_workspace`、`_session_id`、`_agent_id` 隐藏字段，构建 `ToolCallContext`。
    /// 这些字段由编排层注入，不出现在工具 Schema 中。
    pub fn from_args(args: &serde_json::Value) -> Self {
        let session_id = args
            .get("_session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let workspace = args
            .get("_workspace")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);

        let agent_paths = if workspace.is_some() || session_id.is_some() {
            Some(AgentPaths {
                agent_id: args
                    .get("_agent_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                workspace,
                ..Default::default()
            })
        } else {
            None
        };

        Self {
            session_id,
            agent_paths,
            subagent_ops: None,
        }
    }

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
    fn tool_call_context_from_args_with_context_fields() {
        let args = serde_json::json!({
            "path": "src/main.rs",
            "_session_id": "abc123",
            "_workspace": "/tmp/project",
            "_agent_id": "global/coder"
        });
        let ctx = ToolCallContext::from_args(&args);
        assert_eq!(ctx.session_id, Some("abc123".to_string()));
        assert!(ctx.agent_paths.is_some());
        let ap = ctx.agent_paths.unwrap();
        assert_eq!(ap.agent_id, Some("global/coder".to_string()));
        assert_eq!(ap.workspace, Some(PathBuf::from("/tmp/project")));
    }

    #[test]
    fn tool_call_context_from_args_without_context_fields() {
        let args = serde_json::json!({
            "path": "src/main.rs"
        });
        let ctx = ToolCallContext::from_args(&args);
        assert!(ctx.session_id.is_none());
        assert!(ctx.agent_paths.is_none());
    }

    #[test]
    fn tool_call_context_workspace_accessor() {
        let args = serde_json::json!({
            "_workspace": "/tmp/project"
        });
        let ctx = ToolCallContext::from_args(&args);
        assert_eq!(ctx.workspace(), Some(Path::new("/tmp/project")));
    }

    #[test]
    fn tool_call_context_task_id_default() {
        let ctx = ToolCallContext::default();
        assert_eq!(ctx.task_id(), "default");
    }

    #[test]
    fn tool_call_context_task_id_from_session() {
        let args = serde_json::json!({
            "_session_id": "my_session"
        });
        let ctx = ToolCallContext::from_args(&args);
        assert_eq!(ctx.task_id(), "my_session");
    }
}
