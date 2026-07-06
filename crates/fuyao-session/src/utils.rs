//! Session 管理工具函数

use fuyao_api::AgentPaths;
use fuyao_provider::registry::get_model;

/// 解析指定模型的上下文窗口大小
///
/// model_id 由调用方传入（从 `agent_ctx.model_config.model_id` 读取当前运行时模型），
/// 不再自行加载配置文件。fallback 值从全局配置 `get_config().session.compression` 读取。
///
/// # Arguments
/// * `model_id` - 当前模型 ID（如 "aliyun/qwen3.6-plus"），None 时直接返回 fallback
/// * `agent_paths` - Agent 路径配置（用于查询模型注册表）
///
/// # Returns
/// 上下文窗口大小（token 数），无法解析时返回 fallback_context
pub fn resolve_context_length(model_id: Option<&str>, agent_paths: &AgentPaths) -> usize {
    let fallback = fuyao_api::get_config().session.compression.fallback_context as usize;
    let Some(id) = model_id else {
        return fallback;
    };
    get_model(id, agent_paths)
        .map(|m| m.limit.context as usize)
        .unwrap_or(fallback)
}
