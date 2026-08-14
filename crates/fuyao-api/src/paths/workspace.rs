//! 工作目录层路径
//!
//! 工作目录层架构：
//! - .fuyao 根目录: `{workspace}/.fuyao/`
//! - Agents 目录: `{workspace}/.fuyao/fuyao-agents/`

use super::global::get_fuyao_agents_dir;
use crate::selection::AgentIdSource;
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

/// 解析 agent_id 字符串为（来源层, 目录名）
///
/// 格式规则：`split_once('/')`，首段为来源，次段为目录名。
/// - 必须带 `/` 且两段均非空（裸名 / 空段即格式错误）
/// - 来源仅认 global / workspace（大小写不敏感——前端可直接用列举侧
///   [`AgentIdSource`](crate::AgentIdSource) 的 PascalCase 序列化值作前缀）
/// - 目录名须为单段名称（不含额外斜杠）
///
/// 数据去向必须显式声明：裸名与未知来源一律报错，禁止隐式选址。
fn parse_agent_id(agent_id: &str) -> Result<(AgentIdSource, &str), String> {
    let (source, name) = match agent_id.split_once('/') {
        Some((s, n)) if !s.is_empty() && !n.is_empty() => (s, n),
        _ => {
            return Err(format!(
                "agent_id 格式错误（应为 global/{{名}} 或 workspace/{{名}}）: {agent_id}"
            ));
        }
    };

    let source = if source.eq_ignore_ascii_case("global") {
        AgentIdSource::Global
    } else if source.eq_ignore_ascii_case("workspace") {
        AgentIdSource::Workspace
    } else {
        return Err(format!(
            "agent_id 来源未知（应为 global 或 workspace，大小写不敏感）: {agent_id}"
        ));
    };

    if name.contains('/') {
        return Err(format!(
            "agent_id 目录名非法（须为单段名称，不含斜杠）: {agent_id}"
        ));
    }

    Ok((source, name))
}

/// 返回 Agent 目录路径
///
/// agent_id 的数据去向由来源前缀唯一决定，无隐式选址：
/// - `global/{名}` → `{fuyao_home}/fuyao-agents/{名}`（忽略 workspace 参数）
/// - `workspace/{名}` → `{workspace}/.fuyao/fuyao-agents/{名}`（必须配 workspace 参数）
///
/// `fuyao_home` 由调用方注入（读 [`AgentPaths`](crate::AgentPaths) 的 `fuyao_home`
/// 字段），路径解析为纯函数、零全局状态。
///
/// # 错误
///
/// 格式非法（裸名 / 未知来源 / 目录名含斜杠）或 workspace 来源缺 workspace 参数时
/// 返回 Err，错误信息面向最终用户，含格式与修正建议。
pub fn get_agent_root(
    agent_id: &str,
    fuyao_home: &Path,
    workspace: Option<&Path>,
) -> Result<PathBuf, String> {
    let (source, name) = parse_agent_id(agent_id)?;

    match source {
        AgentIdSource::Global => Ok(get_fuyao_agents_dir(fuyao_home).join(name)),
        AgentIdSource::Workspace => {
            let ws = workspace.ok_or_else(|| {
                format!(
                    "agent_id \"{agent_id}\" 为 workspace 来源但未提供 workspace 参数，\
                     项目层数据目录无法定位；请提供工作目录或改用 global 来源"
                )
            })?;
            Ok(get_workspace_agents_dir(ws).join(name))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `global/{名}`：全局层，用注入的 fuyao_home，忽略 workspace 参数
    #[test]
    fn get_agent_root_global_prefix_uses_injected_home() {
        let home = PathBuf::from("/tmp/home");
        let root = get_agent_root("global/coder", &home, None).unwrap();
        assert_eq!(root, home.join("fuyao-agents").join("coder"));

        // 提供 workspace 也不影响：global 前缀强制全局层
        let ws = PathBuf::from("/tmp/project");
        let root_with_ws = get_agent_root("global/coder", &home, Some(&ws)).unwrap();
        assert_eq!(root_with_ws, home.join("fuyao-agents").join("coder"));
    }

    /// `workspace/{名}` + workspace=Some：强制工作目录层 `{ws}/.fuyao/fuyao-agents/{名}`
    #[test]
    fn get_agent_root_workspace_prefix_uses_workspace_layer() {
        let home = PathBuf::from("/tmp/home");
        let ws = PathBuf::from("/tmp/project");
        let root = get_agent_root("workspace/coder", &home, Some(&ws)).unwrap();
        assert_eq!(root, ws.join(".fuyao").join("fuyao-agents").join("coder"));
    }

    /// `workspace/{名}` + workspace=None：报错，信息含修正建议（不再隐式回退）
    #[test]
    fn get_agent_root_workspace_prefix_without_workspace_errors() {
        let err = get_agent_root("workspace/coder", Path::new("/tmp/home"), None).unwrap_err();
        assert!(err.contains("workspace/coder"), "{err}");
        assert!(err.contains("global"), "应建议改用 global 来源：{err}");
    }

    /// 前缀大小写不敏感（Global/ / WORKSPACE/ 等价命中对应层），路径结果一致
    #[test]
    fn get_agent_root_prefix_case_insensitive() {
        let home = PathBuf::from("/tmp/home");
        let ws = PathBuf::from("/tmp/project");

        let pascal_global = get_agent_root("Global/coder", &home, Some(&ws)).unwrap();
        assert_eq!(
            pascal_global,
            home.join("fuyao-agents").join("coder"),
            "Global/ 前缀应命中全局层"
        );

        let upper_ws = get_agent_root("WORKSPACE/coder", &home, Some(&ws)).unwrap();
        assert_eq!(
            upper_ws,
            ws.join(".fuyao").join("fuyao-agents").join("coder"),
            "WORKSPACE/ 前缀应命中工作目录层"
        );
    }

    /// 裸名（无斜杠）报错，信息含正确格式
    #[test]
    fn get_agent_root_bare_name_errors() {
        let err = get_agent_root("coder", Path::new("/tmp/home"), None).unwrap_err();
        assert!(err.contains("global/{名}"), "{err}");
        assert!(err.contains("workspace/{名}"), "{err}");
    }

    /// 未知来源 / 空目录名 / 多段目录名均报错
    #[test]
    fn get_agent_root_illegal_forms_error() {
        let home = Path::new("/tmp/home");

        let unknown = get_agent_root("test/coder", home, None).unwrap_err();
        assert!(unknown.contains("来源未知"), "{unknown}");

        let empty_name = get_agent_root("global/", home, None).unwrap_err();
        assert!(empty_name.contains("格式错误"), "{empty_name}");

        let nested = get_agent_root("global/a/b", home, None).unwrap_err();
        assert!(nested.contains("目录名非法"), "{nested}");
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
