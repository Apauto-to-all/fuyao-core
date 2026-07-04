//! 工具调用上下文
//!
//! 工具执行时的上下文信息，由编排层注入。
//! 不出现在工具 Schema 中，LLM 无法访问。
//!
//! 对应 Python 的 `fuyao.tools.types.ToolCallContext`。

use crate::AgentPaths;
use std::path::{Path, PathBuf};

/// 工具调用上下文
///
/// 工具执行时的上下文信息，由编排层注入。
/// 不出现在工具 Schema 中，LLM 无法访问。
#[derive(Debug, Clone, Default)]
pub struct ToolCallContext {
    /// 会话 ID
    pub session_id: Option<String>,

    /// Agent 三层目录身份证明
    pub agent_paths: Option<AgentPaths>,
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
            })
        } else {
            None
        };

        Self {
            session_id,
            agent_paths,
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
