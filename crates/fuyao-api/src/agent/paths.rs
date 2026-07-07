//! Agent 三层目录的身份证明

use crate::paths::{LayeredPaths, get_agent_root, get_fuyao_home, get_workspace_root};
use std::path::PathBuf;

/// Agent 三层目录的身份证明
///
/// 携带 agent_id 和 workspace，为各模块提供三层路径解析能力。
/// 不可变模型，切换 workspace 时创建新实例。
///
/// 对应 Python 的 `fuyao.types.AgentPaths`。
#[derive(Debug, Clone, Default)]
pub struct AgentPaths {
    /// Agent 标识符，如 "global/coder"、"workspace/coder"
    pub agent_id: Option<String>,

    /// 工作目录路径
    pub workspace: Option<PathBuf>,

    /// 额外资源目录（插件根目录等），运行时由应用层填充
    ///
    /// 各资源类型按约定子目录解析：skills → `{dir}/skills/`，未来扩展同理。
    /// 目前只 skills_paths() 消费它。
    pub extra_dirs: Vec<PathBuf>,
}

impl AgentPaths {
    /// 配置文件分层路径（fuyao.toml）
    pub fn config_paths(&self) -> LayeredPaths {
        LayeredPaths {
            global_: Some(get_fuyao_home().join("fuyao.toml")),
            agent: self.agent_root().map(|p| p.join("fuyao.toml")),
            workspace: self
                .workspace
                .as_ref()
                .map(|ws| get_workspace_root(ws).join("fuyao.toml")),
            ..Default::default()
        }
    }

    /// 环境变量文件分层路径（.env）
    pub fn env_paths(&self) -> LayeredPaths {
        LayeredPaths {
            global_: Some(get_fuyao_home().join(".env")),
            agent: self.agent_root().map(|p| p.join(".env")),
            workspace: self
                .workspace
                .as_ref()
                .map(|ws| get_workspace_root(ws).join(".env")),
            ..Default::default()
        }
    }

    /// Sessions 数据库分层路径
    pub fn sessions_db_paths(&self) -> LayeredPaths {
        LayeredPaths {
            global_: if self.agent_id.is_none() {
                Some(get_fuyao_home().join("sessions").join("sessions.db"))
            } else {
                None
            },
            agent: self
                .agent_root()
                .map(|p| p.join("sessions").join("sessions.db")),
            workspace: None,
            ..Default::default()
        }
    }

    /// Sessions 数据库路径（永远有效）
    ///
    /// 优先级：Agent 层 或者 全局层（只能提供一个）
    /// - 有 agent_id → Agent 层路径
    /// - 无 agent_id → 全局层路径
    ///
    /// # Panics
    ///
    /// 如果路径计算结果为 None（不应发生的设计异常）
    pub fn sessions_db_path(&self) -> PathBuf {
        let paths = self.sessions_db_paths();
        paths
            .agent
            .or(paths.global_)
            .expect("sessions_db_path 计算失败：agent 和 global 均为 None")
    }

    /// Skills 分层路径
    ///
    /// 额外目录（extra_dirs）下的 `skills/` 子目录作为最低优先级来源，
    /// 用于支持插件等提供的 Skills。
    pub fn skills_paths(&self) -> LayeredPaths {
        // 从每个额外目录解析 skills 子目录，存在的才加入
        let extra: Vec<PathBuf> = self
            .extra_dirs
            .iter()
            .map(|d| d.join("skills"))
            .filter(|d| d.is_dir())
            .collect();

        LayeredPaths {
            global_: Some(get_fuyao_home().join("skills")),
            agent: self.agent_root().map(|p| p.join("skills")),
            workspace: self
                .workspace
                .as_ref()
                .map(|ws| get_workspace_root(ws).join("skills")),
            extra,
        }
    }

    /// 插件分层路径
    pub fn plugins_paths(&self) -> LayeredPaths {
        LayeredPaths {
            global_: Some(get_fuyao_home().join("plugins")),
            agent: self.agent_root().map(|p| p.join("plugins")),
            workspace: self
                .workspace
                .as_ref()
                .map(|ws| get_workspace_root(ws).join("plugins")),
            ..Default::default()
        }
    }

    /// Agent 系统提示词分层路径（system.md）
    pub fn system_md_paths(&self) -> LayeredPaths {
        LayeredPaths {
            global_: if self.agent_id.is_none() {
                Some(get_fuyao_home().join("system.md"))
            } else {
                None
            },
            agent: self.agent_root().map(|p| p.join("system.md")),
            workspace: None,
            ..Default::default()
        }
    }

    /// AGENTS.md 分层路径（项目上下文）
    pub fn agents_md_paths(&self) -> LayeredPaths {
        LayeredPaths {
            global_: Some(get_fuyao_home().join("AGENTS.md")),
            agent: self.agent_root().map(|p| p.join("AGENTS.md")),
            workspace: self.workspace.as_ref().map(|ws| ws.join("AGENTS.md")),
            ..Default::default()
        }
    }

    /// Agent 根目录路径
    pub fn agent_root(&self) -> Option<PathBuf> {
        self.agent_id
            .as_deref()
            .map(|id| get_agent_root(id, self.workspace.as_deref()))
    }

    /// 生成缓存 key（用于 Provider/Model 注册表）
    pub fn cache_key(&self) -> String {
        let agent_id = self.agent_id.as_deref().unwrap_or("");
        let workspace = self
            .workspace
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        format!("{agent_id}|{workspace}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_paths_default_has_none_values() {
        let paths = AgentPaths::default();
        assert!(paths.agent_id.is_none());
        assert!(paths.workspace.is_none());
    }

    #[test]
    fn agent_paths_config_paths_returns_layered() {
        let paths = AgentPaths {
            agent_id: Some("global/coder".to_string()),
            workspace: Some(PathBuf::from("/tmp/project")),
            ..Default::default()
        };
        let cp = paths.config_paths();
        assert!(cp.global_.is_some());
        assert!(cp.agent.is_some());
        assert!(cp.workspace.is_some());
    }

    #[test]
    fn agent_paths_sessions_db_path_returns_path() {
        let paths = AgentPaths::default();
        let db_path = paths.sessions_db_path();
        assert!(db_path.to_string_lossy().contains("sessions.db"));
    }

    #[test]
    fn agent_paths_agent_root_returns_path() {
        let paths = AgentPaths {
            agent_id: Some("global/coder".to_string()),
            workspace: None,
            ..Default::default()
        };
        let root = paths.agent_root();
        assert!(root.is_some());
        assert!(root.unwrap().to_string_lossy().ends_with("coder"));
    }

    #[test]
    fn agent_paths_agent_root_none_without_id() {
        let paths = AgentPaths::default();
        assert!(paths.agent_root().is_none());
    }

    #[test]
    fn agent_paths_cache_key_format() {
        let paths = AgentPaths {
            agent_id: Some("global/coder".to_string()),
            workspace: Some(PathBuf::from("/tmp/project")),
            ..Default::default()
        };
        let key = paths.cache_key();
        assert_eq!(key, "global/coder|/tmp/project");
    }

    #[test]
    fn agent_paths_cache_key_default() {
        let paths = AgentPaths::default();
        assert_eq!(paths.cache_key(), "|");
    }

    #[test]
    fn agent_paths_default_has_empty_extra_dirs() {
        let paths = AgentPaths::default();
        assert!(paths.extra_dirs.is_empty());
    }

    #[test]
    fn skills_paths_resolves_extra_dirs() {
        let temp = std::env::temp_dir().join("fuyao_test_skills_extra");
        let plugin_root = temp.join("my-plugin");
        let skills_dir = plugin_root.join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();

        let paths = AgentPaths {
            extra_dirs: vec![plugin_root.clone(), temp.join("no-skills-plugin")],
            ..Default::default()
        };
        let sp = paths.skills_paths();
        // 只有 my-plugin 有 skills 子目录；no-skills-plugin 没有，被跳过
        assert_eq!(sp.extra.len(), 1);
        assert!(sp.extra[0].ends_with("skills"));
        assert!(sp.extra[0].starts_with(&plugin_root));

        std::fs::remove_dir_all(&temp).ok();
    }
}
