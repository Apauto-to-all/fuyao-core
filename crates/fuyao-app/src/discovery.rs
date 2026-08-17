//! 选择支持模块：列举可选 agent_id / Agent 定义 / model
//!
//! 提供两类入口：
//! - [`list_agent_ids`]：模块级函数，接收 [`AgentPaths`] 扫描 `fuyao-agents/`，
//!   不依赖引擎运行时，供应用层在 `start` 前选择数据隔离身份。
//! - [`Discovery`]：与 [`crate::App`]（引擎运行时）、[`crate::SessionManager`]（会话检索）
//!   同级的门面，持有 `start` 时注入的 [`AgentPaths`]，
//!   `list_primary_definitions` / `list_subagent_definitions` / `list_models`
//!   零参数——路径身份启动时定下一次，调用方无需二次传参。

use std::collections::HashMap;

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
/// 持有 [`AgentPaths`]（`start` 时注入），聚合「列举可选项」能力：
/// - [`Discovery::list_primary_definitions`]：列举主 Agent 定义（会话人格，仅 Primary）
/// - [`Discovery::list_subagent_definitions`]：列举子代理定义（仅 Subagent）
/// - [`Discovery::list_models`]：列举 model（来自 Provider 注册缓存）
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

    /// 列举可选 model（来自 Provider / Model 注册缓存，仅启动后有内容）
    ///
    /// 启动前（未跑 [`crate::init_engine`]）注册缓存为空，返回空列表。
    /// `id` 为纯模型名、`provider_id` 独立字段；调用方按需拼成 `provider_id/id` 设给
    /// model_id。`provider_name`（供应商显示名）来自同链注册的 Provider 配置缓存，
    /// 仅供展示，不参与身份与路由。
    pub fn list_models(&self) -> Vec<ModelOption> {
        // Provider 注册缓存建 id → 显示名映射；key 两边均为注册时的小写化形态，直接命中
        let provider_names: HashMap<String, String> =
            fuyao_provider::list_providers(&self.agent_paths)
                .into_iter()
                .map(|(id, provider)| (id, provider.name))
                .collect();
        fuyao_provider::list_models(&self.agent_paths)
            .into_iter()
            .map(|(full_id, model)| {
                // 缓存 key 形如 "provider/model"，拆成独立 provider 与纯模型 id
                let (provider, id) = match full_id.split_once('/') {
                    Some((p, i)) => (p.to_string(), i.to_string()),
                    None => (String::new(), full_id),
                };
                // Provider 与 Model 在装配链同批注册，正常必命中；缓存异常缺 Provider
                // 时退回 id，保证显示名字段始终有值
                let provider_name = provider_names
                    .get(&provider)
                    .cloned()
                    .unwrap_or_else(|| provider.clone());
                ModelOption {
                    id,
                    provider_id: provider,
                    provider_name,
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

    /// 构造仅含必填展示信息的最小 Provider 配置
    fn test_provider(name: &str) -> fuyao_api::Provider {
        fuyao_api::Provider {
            name: name.to_string(),
            ..Default::default()
        }
    }

    /// 构造最小 Model 配置（元信息取默认值）
    fn test_model(name: &str) -> fuyao_api::Model {
        fuyao_api::Model {
            name: name.to_string(),
            cost: Default::default(),
            limit: Default::default(),
            reasoning_efforts: vec![],
            modalities: Default::default(),
        }
    }

    /// 注册 Provider 与 Model 后列举，provider_name 应注入 Provider 的显示名
    #[test]
    fn list_models_injects_provider_display_name() {
        let temp = tempfile::tempdir().unwrap();
        let agent_paths = AgentPaths {
            fuyao_home: temp.path().to_path_buf(),
            agent_id: Some("global/injects_name".to_string()),
            ..Default::default()
        };
        let cache_key = fuyao_provider::agent_paths_cache_key(&agent_paths);
        fuyao_provider::register_provider("sensenova", test_provider("商汤 SenseNova"), &cache_key);
        fuyao_provider::register_model("sensenova/glm-5.2", test_model("glm-5.2"), &cache_key);

        let models = Discovery::new(agent_paths.clone()).list_models();

        assert_eq!(models.len(), 1, "应列举 1 个 model");
        assert_eq!(models[0].id, "glm-5.2", "id 为纯模型名");
        assert_eq!(
            models[0].provider_id, "sensenova",
            "provider_id 为供应商 id"
        );
        assert_eq!(
            models[0].provider_name, "商汤 SenseNova",
            "provider_name 应取 Provider 注册配置的显示名"
        );

        fuyao_provider::clear_cache(&agent_paths);
    }

    /// Model 对应的 Provider 未注册（缓存异常缺口）时，provider_name 应回退为 id
    #[test]
    fn list_models_missing_provider_falls_back_to_id() {
        let temp = tempfile::tempdir().unwrap();
        let agent_paths = AgentPaths {
            fuyao_home: temp.path().to_path_buf(),
            agent_id: Some("global/fallback_id".to_string()),
            ..Default::default()
        };
        let cache_key = fuyao_provider::agent_paths_cache_key(&agent_paths);
        // 仅注册 model，不注册 provider，制造 Provider 缓存缺口
        fuyao_provider::register_model(
            "orphan/lonely-model",
            test_model("lonely-model"),
            &cache_key,
        );

        let models = Discovery::new(agent_paths.clone()).list_models();

        assert_eq!(models.len(), 1, "应列举 1 个 model");
        assert_eq!(
            models[0].provider_name, "orphan",
            "Provider 缺失时 provider_name 应回退为供应商 id"
        );

        fuyao_provider::clear_cache(&agent_paths);
    }
}
