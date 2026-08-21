//! 选择支持模块：列举可选 agent_id / Agent 定义
//!
//! 提供两类入口：
//! - [`list_agent_ids`]：模块级函数，接收 [`AgentPaths`] 扫描 `fuyao-agents/`，
//!   不依赖引擎运行时，供应用层在 `start` 前选择数据隔离身份。
//! - [`Discovery`]：与 [`crate::App`]（引擎运行时）、[`crate::SessionManager`]（会话检索）
//!   同级的门面，持有 `start` 时注入的 [`AgentPaths`]，
//!   `list_primary_definitions` / `list_subagent_definitions` 零参数——路径身份
//!   启动时定下一次，调用方无需二次传参。
//!
//! model 列举属供应商管理域，见 [`crate::ProviderManager::list_models`]。

use fuyao_api::{AgentIdOption, AgentPaths, DefinitionOption};
use fuyao_prompt::AgentRegistry;

/// 列举可选 agent_id（启动前可用）
///
/// 接收应用层构造的 [`AgentPaths`]，按其 workspace / fuyao_home 扫描 `fuyao-agents/`，
/// 返回所有可选 agent_id。不依赖引擎运行时，供应用层在 [`crate::start`] 前选择
/// 数据隔离身份。启动后同样可调（重新扫描，反映用户新建的 agent 目录）。
///
/// 路径身份由调用方提供：应用层用同一份 `AgentPaths` 先列 id、再造 `EngineParams`
/// 启动，保证列举基准与实际启动基准一致。
pub fn list_agent_ids(paths: &AgentPaths) -> Vec<AgentIdOption> {
    AgentRegistry::new(paths.workspace.clone(), paths.fuyao_home.clone()).list_all_ids()
}

/// 选择支持门面
///
/// 持有 [`AgentPaths`]（`start` 时注入），聚合「列举可选项」能力：
/// - [`Discovery::list_primary_definitions`]：列举主 Agent 定义（会话人格，仅 Primary）
/// - [`Discovery::list_subagent_definitions`]：列举子代理定义（仅 Subagent）
///
/// 路径身份在引擎启动时定下一次，所有查询共用，调用方不再传任何路径参数。
pub struct Discovery {
    /// 启动时注入的路径身份（含 agent_id / workspace / fuyao_home / extra_dirs）
    agent_paths: AgentPaths,
}

impl Discovery {
    /// 构造选择支持门面（持有启动时的路径身份）
    pub(crate) fn new(agent_paths: AgentPaths) -> Self {
        Self { agent_paths }
    }

    /// 列举可选主 Agent 定义（会话人格专用，仅 Primary 模式，四层目录 + 内置）
    ///
    /// Subagent 模式定义不出现（专职供子代理工具派生），列表里能选中的定义
    /// 设为会话人格必然生效。定义扫描覆盖 workspace / agent / global / extra
    /// 四层外加内置定义，路径身份由启动时注入的 [`AgentPaths`] 提供，调用方无需传参。
    pub fn list_primary_definitions(&self) -> Vec<DefinitionOption> {
        fuyao_prompt::list_primary_definitions(&self.agent_paths)
    }

    /// 列举可用子代理定义（仅 Subagent 模式，四层目录 + 内置）
    ///
    /// 子代理工具派生的候选全集（`subagent_type` 取值域），与主代理列举按
    /// mode 互斥。路径身份由启动时注入的 [`AgentPaths`] 提供，调用方无需传参。
    pub fn list_subagent_definitions(&self) -> Vec<DefinitionOption> {
        fuyao_prompt::list_subagent_definitions(&self.agent_paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 空 fuyao_home（无 fuyao-agents 目录）应返回空列表，不 panic
    #[test]
    fn list_agent_ids_empty_home_returns_empty() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths {
            fuyao_home: temp.path().to_path_buf(),
            ..Default::default()
        };

        assert!(
            list_agent_ids(&paths).is_empty(),
            "空 fuyao_home 应返回空 agent_id 列表"
        );
    }

    /// 全局层 fuyao-agents/ 下存在文件夹时，应返回纯名 id + source=Global
    #[test]
    fn list_agent_ids_global_folder_pure_name() {
        let temp = tempfile::tempdir().unwrap();
        // 准备全局层 agent 目录：{fuyao_home}/fuyao-agents/coder
        std::fs::create_dir_all(temp.path().join("fuyao-agents").join("coder")).unwrap();
        let paths = AgentPaths {
            fuyao_home: temp.path().to_path_buf(),
            ..Default::default()
        };

        let ids = list_agent_ids(&paths);

        assert_eq!(ids.len(), 1, "应列举 1 个 agent_id");
        assert_eq!(ids[0].id, "coder", "id 为纯文件夹名（无前缀）");
        assert_eq!(ids[0].source, fuyao_api::AgentIdSource::Global);
    }

    /// 两个定义列举按 mode 互斥：主代理列表仅 default（Primary），
    /// 子代理列表仅 explore / executor（Subagent），无交集
    #[test]
    fn list_definitions_split_by_mode() {
        let temp = tempfile::tempdir().unwrap();
        let agent_paths = AgentPaths {
            fuyao_home: temp.path().to_path_buf(),
            ..Default::default()
        };
        let discovery = Discovery::new(agent_paths);

        let primary = discovery.list_primary_definitions();
        let subagent = discovery.list_subagent_definitions();

        let primary_ids: Vec<&str> = primary.iter().map(|d| d.id.as_str()).collect();
        let subagent_ids: Vec<&str> = subagent.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(primary_ids, vec!["default"], "主代理列表仅内置 default");
        assert_eq!(
            subagent_ids,
            vec!["executor", "explore"],
            "子代理列表仅内置 explore / executor（按 id 升序）"
        );
        assert!(
            primary_ids.iter().all(|id| !subagent_ids.contains(id)),
            "两个列表按 mode 互斥，无交集"
        );
    }
}
