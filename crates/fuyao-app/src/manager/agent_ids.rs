//! agent_id 列举（启动前可用）
//!
//! 模块级函数，接收 [`AgentPaths`] 扫描 `fuyao-agents/`，不依赖引擎运行时，
//! 供应用层在 `start` 前选择数据隔离身份。

use fuyao_api::{AgentIdOption, AgentPaths};
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
}
