//! Agent 三层目录的身份证明

use crate::paths::{LayeredPaths, get_agent_root, get_fuyao_home, get_workspace_root};
use std::path::PathBuf;

/// Agent 三层目录的身份证明
///
/// 携带 agent_id、workspace 和 fuyao_home，为各模块提供三层路径解析能力。
/// `fuyao_home` 构造时注入一次，所有路径方法读 `self.fuyao_home`（纯函数，零全局状态）。
/// 不可变模型，切换 workspace 时创建新实例。
#[derive(Debug, Clone)]
pub struct AgentPaths {
    /// Agent 标识符，如 "global/coder"、"workspace/coder"
    pub agent_id: Option<String>,

    /// 工作目录路径
    pub workspace: Option<PathBuf>,

    /// 额外资源目录（插件根目录等），运行时由应用层填充
    ///
    /// 各资源类型按约定子目录解析：
    /// - skills → `{dir}/skills/`
    /// - agents 定义 → `{dir}/agents/`
    /// - 补充指令 → `{dir}/instructions/`
    pub extra_dirs: Vec<PathBuf>,

    /// 全局基准路径（~/.fuyao），构造时注入一次
    ///
    /// 所有路径方法读此字段而非 `get_fuyao_home()`，使路径解析成为纯函数。
    /// `Default` 内部调用 `get_fuyao_home()`，现有 `AgentPaths::default()` 零回归。
    /// 测试可直接赋值实现 per-instance 隔离，无需操纵环境变量。
    pub fuyao_home: PathBuf,
}

impl Default for AgentPaths {
    fn default() -> Self {
        Self {
            agent_id: None,
            workspace: None,
            extra_dirs: Vec::new(),
            fuyao_home: get_fuyao_home(),
        }
    }
}

impl AgentPaths {
    /// 以当前工作目录为 workspace 构造
    ///
    /// 应用层入口（cli / tui / example）最常用的初始化方式：把进程当前目录
    /// 作为 workspace 层，使其 `.fuyao/` 下的 skills、agents 定义、instructions、
    /// 项目级 `fuyao.toml` 等资源都能被分层路径体系解析到。
    ///
    /// 与 [`Default`] 的区别：`Default` 的 `workspace` 为 `None`，workspace 层被
    /// 完全跳过，仅能看到 global 层；`from_cwd` 保证 workspace 层生效。
    /// 当前目录不可读时回退为 [`Default`]（仅 global 层），避免启动直接失败。
    pub fn from_cwd() -> Self {
        match std::env::current_dir() {
            Ok(ws) => Self {
                workspace: Some(ws),
                ..Self::default()
            },
            Err(_) => Self::default(),
        }
    }
    /// 配置文件分层路径（fuyao.toml）
    pub fn config_paths(&self) -> LayeredPaths {
        LayeredPaths {
            global_: Some(self.fuyao_home.join("fuyao.toml")),
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
            global_: Some(self.fuyao_home.join(".env")),
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
                Some(self.fuyao_home.join("sessions").join("sessions.db"))
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

    /// 日志目录路径（永远有效）
    ///
    /// 选址对齐 `sessions_db_path`：日志随独立 Agent 隔离，与 sessions 同域。
    /// - 有 agent_id → Agent 层 `{agent_root}/logs/`
    /// - 无 agent_id → 全局层 `~/.fuyao/logs/`
    ///
    /// workspace 层不参与（与 sessions.db 一致）。目录由 `init_logging` 按需创建。
    pub fn logs_dir(&self) -> PathBuf {
        match self.agent_root() {
            Some(root) => root.join("logs"),
            None => self.fuyao_home.join("logs"),
        }
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
            global_: Some(self.fuyao_home.join("skills")),
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
            global_: Some(self.fuyao_home.join("plugins")),
            agent: self.agent_root().map(|p| p.join("plugins")),
            workspace: self
                .workspace
                .as_ref()
                .map(|ws| get_workspace_root(ws).join("plugins")),
            ..Default::default()
        }
    }

    /// Agent 定义文件分层路径（`agents/{name}.md`）
    ///
    /// 集中定义库，按 name 匹配单个定义文件。三层优先级（不含 agent 层）：
    /// - workspace: `{workspace}/.fuyao/agents/{name}.md`
    /// - global: `~/.fuyao/agents/{name}.md`
    /// - extra: `{插件根}/agents/{name}.md`（仅保留存在的文件）
    ///
    /// 不用 agent 层：agent_root 是独立 Agent 的数据隔离目录（sessions/config/provider），
    /// 定义库与 agent_id 隔离体系正交，不应塞进每个独立 Agent 的数据目录。
    ///
    /// 调用方用 `first_exists()` 取首个命中。本期 name 固定为 `"default"`。
    pub fn agents_def_paths(&self, name: &str) -> LayeredPaths {
        let file_name = format!("{name}.md");

        // extra：插件根下的 agents/{name}.md，仅保留存在的文件
        let extra: Vec<PathBuf> = self
            .extra_dirs
            .iter()
            .map(|d| d.join("agents").join(&file_name))
            .filter(|p| p.is_file())
            .collect();

        LayeredPaths {
            global_: Some(self.fuyao_home.join("agents").join(&file_name)),
            agent: None,
            workspace: self
                .workspace
                .as_ref()
                .map(|ws| get_workspace_root(ws).join("agents").join(&file_name)),
            extra,
        }
    }

    /// 补充指令目录分层路径（`instructions/`）
    ///
    /// 四层优先级，仿 `skills_paths` 结构。每个目录下所有 `*.md` 全量拼接进补充区：
    /// - workspace: `{workspace}/.fuyao/instructions/`
    /// - agent: `{agent_root}/instructions/`
    /// - global: `~/.fuyao/instructions/`
    /// - extra: `{插件根}/instructions/`（仅保留存在的目录）
    ///
    /// 调用方用 `merge_exists()` 取所有存在的目录，逐一扫描 `*.md`。
    pub fn instructions_paths(&self) -> LayeredPaths {
        let extra: Vec<PathBuf> = self
            .extra_dirs
            .iter()
            .map(|d| d.join("instructions"))
            .filter(|d| d.is_dir())
            .collect();

        LayeredPaths {
            global_: Some(self.fuyao_home.join("instructions")),
            agent: self.agent_root().map(|p| p.join("instructions")),
            workspace: self
                .workspace
                .as_ref()
                .map(|ws| get_workspace_root(ws).join("instructions")),
            extra,
        }
    }

    /// AGENTS.md 分层路径（项目上下文）
    pub fn agents_md_paths(&self) -> LayeredPaths {
        LayeredPaths {
            global_: Some(self.fuyao_home.join("AGENTS.md")),
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
    fn from_cwd_sets_workspace_to_current_dir() {
        // from_cwd：workspace 应等于 std::env::current_dir，使 workspace 层生效
        let paths = AgentPaths::from_cwd();
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(paths.workspace.as_ref(), Some(&cwd));
        assert!(paths.agent_id.is_none());
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
    fn logs_dir_without_agent_id_uses_global_layer() {
        let paths = AgentPaths {
            fuyao_home: PathBuf::from("/tmp/home"),
            ..Default::default()
        };
        assert_eq!(paths.logs_dir(), paths.fuyao_home.join("logs"));
    }

    #[test]
    fn logs_dir_with_agent_id_uses_agent_root() {
        let paths = AgentPaths {
            agent_id: Some("global/coder".to_string()),
            fuyao_home: PathBuf::from("/tmp/home"),
            ..Default::default()
        };
        let expected = paths.agent_root().unwrap().join("logs");
        assert_eq!(paths.logs_dir(), expected);
        // 落在 agent_root 下而非全局 home 根
        assert!(paths.logs_dir().starts_with(paths.agent_root().unwrap()));
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

    #[test]
    fn agents_def_paths_returns_layered_with_name() {
        let paths = AgentPaths {
            workspace: Some(PathBuf::from("/tmp/project")),
            ..Default::default()
        };
        let ap = paths.agents_def_paths("default");
        // global 层
        assert!(ap.global_.is_some());
        assert!(ap.global_.as_ref().unwrap().ends_with("agents/default.md"));
        // workspace 层
        assert!(ap.workspace.is_some());
        assert!(
            ap.workspace
                .as_ref()
                .unwrap()
                .ends_with(".fuyao/agents/default.md")
        );
        // agent 层不用
        assert!(ap.agent.is_none());
    }

    #[test]
    fn agents_def_paths_no_agent_layer_regardless_of_agent_id() {
        // 即使有 agent_id，agents 定义也不走 agent 层（与 agent_id 隔离体系正交）
        let paths = AgentPaths {
            agent_id: Some("global/coder".to_string()),
            ..Default::default()
        };
        let ap = paths.agents_def_paths("default");
        assert!(ap.agent.is_none());
        assert!(ap.global_.is_some());
    }

    #[test]
    fn agents_def_paths_extra_filters_existing_files() {
        let temp = std::env::temp_dir().join("fuyao_test_agents_def_extra");
        let plugin_a = temp.join("plugin-a");
        let plugin_b = temp.join("plugin-b");
        // plugin-a 有 agents/default.md
        std::fs::create_dir_all(plugin_a.join("agents")).unwrap();
        std::fs::write(plugin_a.join("agents").join("default.md"), "# A").unwrap();
        // plugin-b 有 agents/ 但没有 default.md
        std::fs::create_dir_all(plugin_b.join("agents")).unwrap();

        let paths = AgentPaths {
            extra_dirs: vec![plugin_a.clone(), plugin_b.clone()],
            ..Default::default()
        };
        let ap = paths.agents_def_paths("default");
        // 只有 plugin-a 的 default.md 存在，plugin-b 被过滤
        assert_eq!(ap.extra.len(), 1);
        assert!(ap.extra[0].starts_with(&plugin_a));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn instructions_paths_returns_four_layers() {
        let paths = AgentPaths {
            agent_id: Some("global/coder".to_string()),
            workspace: Some(PathBuf::from("/tmp/project")),
            ..Default::default()
        };
        let ip = paths.instructions_paths();
        // 四层齐全
        assert!(ip.global_.is_some());
        assert!(ip.agent.is_some());
        assert!(ip.workspace.is_some());
        assert!(ip.global_.as_ref().unwrap().ends_with("instructions"));
        assert!(ip.agent.as_ref().unwrap().ends_with("instructions"));
        assert!(
            ip.workspace
                .as_ref()
                .unwrap()
                .ends_with(".fuyao/instructions")
        );
    }

    #[test]
    fn instructions_paths_extra_filters_existing_dirs() {
        let temp = std::env::temp_dir().join("fuyao_test_instructions_extra");
        let plugin_a = temp.join("plugin-a");
        let plugin_b = temp.join("plugin-b");
        std::fs::create_dir_all(plugin_a.join("instructions")).unwrap();
        // plugin-b 没有 instructions 目录

        let paths = AgentPaths {
            extra_dirs: vec![plugin_a.clone(), plugin_b.clone()],
            ..Default::default()
        };
        let ip = paths.instructions_paths();
        assert_eq!(ip.extra.len(), 1);
        assert!(ip.extra[0].starts_with(&plugin_a));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn agent_paths_default_has_nonempty_fuyao_home() {
        let paths = AgentPaths::default();
        assert!(!paths.fuyao_home.as_os_str().is_empty());
    }

    #[test]
    fn injected_fuyao_home_used_by_path_methods() {
        let temp = std::env::temp_dir().join("fuyao_test_injected_home");
        std::fs::create_dir_all(&temp).unwrap();

        let paths = AgentPaths {
            fuyao_home: temp.clone(),
            ..Default::default()
        };

        // config_paths 应使用注入的 home
        let cp = paths.config_paths();
        assert_eq!(cp.global_, Some(temp.join("fuyao.toml")));

        // agents_def_paths 应使用注入的 home
        let ap = paths.agents_def_paths("default");
        assert_eq!(ap.global_, Some(temp.join("agents").join("default.md")));

        // agents_md_paths 应使用注入的 home
        let amp = paths.agents_md_paths();
        assert_eq!(amp.global_, Some(temp.join("AGENTS.md")));

        std::fs::remove_dir_all(&temp).ok();
    }
}
