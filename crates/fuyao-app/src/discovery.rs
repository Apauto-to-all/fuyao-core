//! 选择支持模块：列举可选 agent_id / Agent 定义 / model
//!
//! 提供两类入口：
//! - [`list_agent_ids`]：模块级函数，接收 [`AgentPaths`] 扫描 `fuyao-agents/`，
//!   不依赖引擎运行时，供应用层在 `start` 前选择数据隔离身份。
//! - [`Discovery`]：与 [`crate::App`]（引擎运行时）、[`crate::SessionManager`]（会话检索）
//!   同级的门面，持有 `start` 时注入的 [`AgentPaths`]，`list_definitions` / `list_models`
//!   零参数——路径身份启动时定下一次，调用方无需二次传参。

use fuyao_api::{AgentIdOption, AgentPaths, DefinitionOption, ModelOption};
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
/// 持有 [`AgentPaths`]（`start` 时注入），聚合两类「列举可选项」能力：
/// - [`Discovery::list_definitions`]：列举 Agent 定义（人格）
/// - [`Discovery::list_models`]：列举 model（来自 Provider 注册缓存）
///
/// 路径身份在引擎启动时定下一次，两个查询共用，调用方不再传任何路径参数。
pub struct Discovery {
    /// 启动时注入的路径身份（含 agent_id / workspace / fuyao_home / extra_dirs）
    agent_paths: AgentPaths,
}

impl Discovery {
    /// 构造选择支持门面（持有启动时的路径身份）
    pub(crate) fn new(agent_paths: AgentPaths) -> Self {
        Self { agent_paths }
    }

    /// 列举可选 Agent 定义（人格，全模式，四层目录 + 内置）
    ///
    /// 定义扫描覆盖 workspace / agent / global / extra 四层外加内置定义，
    /// 路径身份由启动时注入的 [`AgentPaths`] 提供，调用方无需传参。
    pub fn list_definitions(&self) -> Vec<DefinitionOption> {
        fuyao_prompt::list_definitions(&self.agent_paths)
    }

    /// 列举可选 model（来自 Provider 注册缓存，仅启动后有内容）
    ///
    /// 启动前（未跑 [`crate::init_engine`]）注册缓存为空，返回空列表。
    /// `id` 为纯模型名、`provider` 独立字段；调用方按需拼成 `provider/id` 设给 model_id。
    pub fn list_models(&self) -> Vec<ModelOption> {
        fuyao_provider::list_models(&self.agent_paths)
            .into_iter()
            .map(|(full_id, model)| {
                // 缓存 key 形如 "provider/model"，拆成独立 provider 与纯模型 id
                let (provider, id) = match full_id.split_once('/') {
                    Some((p, i)) => (p.to_string(), i.to_string()),
                    None => (String::new(), full_id),
                };
                ModelOption {
                    id,
                    provider,
                    model,
                }
            })
            .collect()
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

    /// 启动前（无 Provider 注册缓存）列举 model 应返回空列表，不 panic
    #[test]
    fn list_models_no_cache_returns_empty() {
        let temp = tempfile::tempdir().unwrap();
        let agent_paths = AgentPaths {
            fuyao_home: temp.path().to_path_buf(),
            ..Default::default()
        };
        let discovery = Discovery::new(agent_paths);

        let models = discovery.list_models();

        assert!(
            models.is_empty(),
            "未跑 init_engine 时缓存为空，应返回空 model 列表"
        );
    }
}
