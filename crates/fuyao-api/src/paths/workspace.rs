//! 工作目录层路径
//!
//! 工作目录层架构：
//! - .fuyao 根目录: `{workspace}/.fuyao/`
//! - Agents 目录: `{workspace}/.fuyao/fuyao-agents/`
//! - Workflow 配置: `{workspace}/.fuyao/workflow.toml`

use super::global::get_fuyao_agents_dir;
use std::path::{Path, PathBuf};

/// 返回工作目录的 .fuyao 根目录路径
///
/// 路径: `{workspace}/.fuyao/`
pub fn get_workspace_root(workspace: &Path) -> PathBuf {
    workspace.join(".fuyao")
}

/// 返回工作目录下 Agent 的根目录路径
///
/// 路径: `{workspace}/.fuyao/fuyao-agents/`
pub fn get_workspace_agents_dir(workspace: &Path) -> PathBuf {
    get_workspace_root(workspace).join("fuyao-agents")
}

/// 返回 Agent 目录路径
///
/// `agent_id` 格式：
/// - `"global/agent-id"` → 强制全局层，忽略 workspace 参数
/// - `"workspace/agent-id"` → 强制工作目录层（需要 workspace 参数）
/// - `"agent-id"` → 默认行为：
///     1. 优先使用已存在的 Agent 目录（工作目录 > 全局）
///     2. 都不存在时，返回全局层（Agent 是全局概念）
///
/// 前缀比较大小写不敏感——`"Global/..."`、`"GLOBAL/..."` 与 `"global/..."` 等价，
/// 使上游直接用来源枚举的序列化值（PascalCase）作前缀时无需额外大小写转换。
pub fn get_agent_root(agent_id: &str, workspace: Option<&Path>) -> PathBuf {
    if let Some((prefix, name)) = agent_id.split_once('/') {
        if prefix.eq_ignore_ascii_case("global") {
            return get_fuyao_agents_dir().join(name);
        }
        if prefix.eq_ignore_ascii_case("workspace")
            && let Some(ws) = workspace
        {
            return get_workspace_agents_dir(ws).join(name);
        }
        return resolve_agent_root(agent_id, workspace);
    }

    resolve_agent_root(agent_id, workspace)
}

/// 解析 agent_id 的实际路径（无前缀或未知前缀）
///
/// 优先使用已存在的目录（工作目录优先级更高），否则返回全局层。
fn resolve_agent_root(agent_id: &str, workspace: Option<&Path>) -> PathBuf {
    if let Some(ws) = workspace {
        let local = get_workspace_agents_dir(ws).join(agent_id);
        if local.exists() {
            return local;
        }
    }
    get_fuyao_agents_dir().join(agent_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_agent_root_global_prefix() {
        let root = get_agent_root("global/coder", None);
        assert!(root.to_string_lossy().contains("fuyao-agents"));
        assert!(root.to_string_lossy().ends_with("coder"));
    }

    #[test]
    fn get_agent_root_workspace_prefix() {
        let ws = PathBuf::from("/tmp/project");
        let root = get_agent_root("workspace/coder", Some(&ws));
        assert!(root.to_string_lossy().contains(".fuyao"));
        assert!(root.to_string_lossy().contains("fuyao-agents"));
        assert!(root.to_string_lossy().ends_with("coder"));
    }

    #[test]
    fn get_agent_root_前缀大小写不敏感() {
        // PascalCase 前缀（与 AgentIdSource 枚举序列化值同形）应与小写前缀等价命中层定位，
        // 上游无需为大小写做额外转换
        let root_global = get_agent_root("Global/coder", None);
        assert!(root_global.to_string_lossy().contains("fuyao-agents"));
        assert!(root_global.to_string_lossy().ends_with("coder"));

        let ws = PathBuf::from("/tmp/project");
        let root_ws = get_agent_root("Workspace/coder", Some(&ws));
        assert!(root_ws.to_string_lossy().contains(".fuyao"));
        assert!(root_ws.to_string_lossy().contains("fuyao-agents"));
        assert!(root_ws.to_string_lossy().ends_with("coder"));
    }

    #[test]
    fn get_agent_root_bare_id_defaults_to_global() {
        let root = get_agent_root("nonexistent_agent_12345", None);
        assert!(root.to_string_lossy().contains("fuyao-agents"));
        assert!(root.to_string_lossy().ends_with("nonexistent_agent_12345"));
    }

    #[test]
    fn get_workspace_root_returns_fuyao_dir() {
        let ws = PathBuf::from("/tmp/project");
        let root = get_workspace_root(&ws);
        assert_eq!(root, PathBuf::from("/tmp/project/.fuyao"));
    }

    #[test]
    fn get_workspace_agents_dir_returns_agents_subdir() {
        let ws = PathBuf::from("/tmp/project");
        let dir = get_workspace_agents_dir(&ws);
        assert_eq!(dir, PathBuf::from("/tmp/project/.fuyao/fuyao-agents"));
    }
}
