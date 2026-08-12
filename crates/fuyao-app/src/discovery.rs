//! 选择支持门面：列举可选 agent_id / Agent 定义 / model
//!
//! 与 [`crate::App`]（引擎运行时）、[`crate::SessionManager`]（会话检索）同级。
//! 仅负责告诉外部有哪些可选项，不执行切换。仅凭路径即可构造，无需引擎，
//! 供启动前的选择器使用；启动后也作为 [`crate::FuyaoApp`] 的字段持有。

use std::path::PathBuf;

use fuyao_api::{AgentIdOption, AgentPaths, DefinitionOption, ModelOption};
use fuyao_prompt::AgentRegistry;

/// 选择支持门面
///
/// 持有 workspace 与 fuyao_home 两份路径基准，聚合三类「列举可选项」能力：
/// - [`Discovery::list_agent_ids`]：列举 agent_id（数据隔离身份）
/// - [`Discovery::list_definitions`]：列举 Agent 定义（人格）
/// - [`Discovery::list_models`]：列举 model（来自 Provider 注册缓存）
///
/// 三类查询都不依赖引擎运行时——agent_id 与 Agent 定义纯文件系统扫描，
/// model 读 Provider 注册缓存（启动后才有内容，启动前为空）。
pub struct Discovery {
    /// 工作目录路径（项目层基准，None 表示仅全局层）
    workspace: Option<PathBuf>,
    /// 全局基准路径（~/.fuyao）
    fuyao_home: PathBuf,
}

impl Discovery {
    /// 构造选择支持门面（无需引擎）
    pub fn new(fuyao_home: PathBuf, workspace: Option<PathBuf>) -> Self {
        Self {
            workspace,
            fuyao_home,
        }
    }

    /// 列举可选 agent_id
    ///
    /// 纯文件夹扫描，启动前后均可调用。id 为纯名（不带 `global/` / `workspace/`
    /// 前缀），来源由 `source` 字段承载；项目层同名覆盖全局层，结果按 id 升序排序。
    pub fn list_agent_ids(&self) -> Vec<AgentIdOption> {
        AgentRegistry::new(self.workspace.clone(), self.fuyao_home.clone()).list_all_ids()
    }

    /// 列举可选 Agent 定义（人格，全模式，四层目录 + 内置）
    ///
    /// 需传入 [`AgentPaths`]——定义扫描覆盖 workspace / agent / global / extra 四层
    /// 外加内置定义，调用方需提供完整的分层路径身份。
    pub fn list_definitions(&self, agent_paths: &AgentPaths) -> Vec<DefinitionOption> {
        fuyao_prompt::list_definitions(agent_paths)
    }

    /// 列举可选 model（来自 Provider 注册缓存，仅启动后有内容）
    ///
    /// 启动前（未跑 [`crate::init_engine`]）注册缓存为空，返回空列表。
    /// `id` 为纯模型名、`provider` 独立字段；调用方按需拼成 `provider/id` 设给 model_id。
    pub fn list_models(&self, agent_paths: &AgentPaths) -> Vec<ModelOption> {
        fuyao_provider::list_models(agent_paths)
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
        let discovery = Discovery::new(temp.path().to_path_buf(), None);

        let ids = discovery.list_agent_ids();

        assert!(ids.is_empty(), "空 fuyao_home 应返回空 agent_id 列表");
    }

    /// 全局层 fuyao-agents/ 下存在文件夹时，应返回纯名 id + source=Global
    #[test]
    fn list_agent_ids_global_folder_pure_name() {
        let temp = tempfile::tempdir().unwrap();
        // 准备全局层 agent 目录：{fuyao_home}/fuyao-agents/coder
        std::fs::create_dir_all(temp.path().join("fuyao-agents").join("coder")).unwrap();

        let discovery = Discovery::new(temp.path().to_path_buf(), None);
        let ids = discovery.list_agent_ids();

        assert_eq!(ids.len(), 1, "应列举 1 个 agent_id");
        assert_eq!(ids[0].id, "coder", "id 为纯文件夹名（无前缀）");
        assert_eq!(ids[0].source, fuyao_api::Source::Global);
    }

    /// 启动前（无 Provider 注册缓存）列举 model 应返回空列表，不 panic
    #[test]
    fn list_models_no_cache_returns_empty() {
        // 用临时目录构造一个独立的 AgentPaths，确保不会命中其他测试注册的缓存
        let temp = tempfile::tempdir().unwrap();
        let agent_paths = AgentPaths {
            agent_id: None,
            workspace: None,
            extra_dirs: Vec::new(),
            fuyao_home: temp.path().to_path_buf(),
        };
        let discovery = Discovery::new(temp.path().to_path_buf(), None);

        let models = discovery.list_models(&agent_paths);

        assert!(
            models.is_empty(),
            "未跑 init_engine 时缓存为空，应返回空 model 列表"
        );
    }
}
