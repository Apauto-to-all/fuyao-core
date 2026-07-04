//! Session 管理工具函数

use fuyao_api::AgentPaths;
use fuyao_provider::registry::get_model;

/// 解析当前模型的上下文窗口大小
///
/// 每次调用都动态获取，支持运行时切换模型。
///
/// # Arguments
/// * `agent_paths` - Agent 路径配置
///
/// # Returns
/// 上下文窗口大小（token 数），默认 128000
pub fn resolve_context_length(agent_paths: &AgentPaths) -> usize {
    // 从配置中获取默认模型的 context length
    let config = match fuyao_config::load_config(agent_paths) {
        Ok(Some(c)) => c,
        _ => return 128_000,
    };

    let model_id = match config.model {
        Some(id) => id,
        None => return 128_000,
    };

    get_model(&model_id, agent_paths)
        .map(|m| m.limit.context as usize)
        .unwrap_or(128_000)
}
