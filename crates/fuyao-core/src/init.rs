//! 引擎初始化 —— SDK 开箱装配入口
//!
//! 封装从 [`AgentContext`] 到可用 `((Engine, EngineHandle))` 的完整初始化流程：
//! 1. 加载 `.env` 环境变量（三层目录）
//! 2. 加载三层 TOML 配置并注册 Provider/Model 到注册表
//! 3. 确定 `model_id`（显式指定 > 配置文件 `model` 字段 > 第一个已注册模型）
//! 4. 创建 [`OpenAIProvider`] 实例（按 `provider_id`）
//! 5. 验证模型存在于注册表
//! 6. 创建 [`Engine`]，返回 `((Engine, EngineHandle))`
//!
//! 典型用法：
//! ```ignore
//! let agent_ctx = AgentContext {
//!     model_id: Some("deepseek/deepseek-v4-flash".to_string()),
//!     ..Default::default()
//! };
//! let (engine, handle) = fuyao_core::init::init_engine(agent_ctx)?;
//! ```

use fuyao_api::{AgentContext, AgentPaths};
use fuyao_config::{load_config, load_env};
use fuyao_provider::openai::OpenAIProvider;
use fuyao_provider::registry::{
    agent_paths_cache_key, get_model, list_models, register_model, register_provider,
};

use crate::{Engine, EngineHandle};

/// 初始化错误
///
/// 每个变体携带可用于排错的上下文（已指定的 model_id、可用模型/Provider 列表等），
/// 错误信息直接面向最终用户，包含明确的修正建议（如检查 API Key 配置）。
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    /// 未配置 `model_id`，且注册表中没有任何可用默认模型
    #[error("未配置 model_id 且无可用默认模型 (指定: {model_id:?}, 可用: {available:?})")]
    NoModelWithDetail {
        /// 调用方显式指定的 model_id（可能为 None）
        model_id: Option<String>,
        /// 当前注册表中所有可用模型的 full_id 列表
        available: Vec<String>,
    },

    /// Provider 创建失败（通常是 API Key 未配置或 provider_id 不存在）
    #[error(
        "Provider 创建失败: {provider_id} (可用模型: {available_providers:?})。请检查 API Key 是否配置（环境变量或 .env 文件）"
    )]
    ProviderNotFoundWithDetail {
        /// 失败的 provider_id（model_id 中 `/` 之前的部分）
        provider_id: String,
        /// 当前注册表中所有可用 Provider 的 full_id 列表
        available_providers: Vec<String>,
    },

    /// 模型信息获取失败（model_id 通过了默认推断，但在注册表中查不到）
    #[error("模型信息获取失败: {0}")]
    ModelInfoFailed(String),

    /// 配置加载失败（TOML 解析错误、IO 错误等）
    #[error("配置加载失败: {0}")]
    ConfigError(String),
}

/// 初始化引擎 —— SDK 开箱装配入口
///
/// 从 [`AgentContext`] 出发，一气呵成完成：环境变量加载 → 配置注册 →
/// model_id 确定 → Provider 创建 → 模型校验 → Engine 装配，返回可直接使用的
/// `((Engine, EngineHandle))`。
///
/// # 参数
/// - `agent_ctx`：Agent 运行时上下文，至少应填充 `agent_paths`（决定三层目录）；
///   若填充 `model_id` 则优先使用，否则按配置文件 `model` 字段 / 第一个已注册模型回退。
///
/// # 错误
/// - [`InitError::NoModelWithDetail`]：未指定 `model_id` 且注册表为空
/// - [`InitError::ProviderNotFoundWithDetail`]：Provider 创建失败（API Key 缺失等）
/// - [`InitError::ModelInfoFailed`]：model_id 在注册表中不存在
/// - [`InitError::ConfigError`]：配置文件加载失败
pub fn init_engine(agent_ctx: AgentContext) -> Result<(Engine, EngineHandle), InitError> {
    let agent_paths = agent_ctx.agent_paths.clone();

    // 1. 加载 .env 环境变量（fuyao-config 模块）
    load_env(&agent_paths);

    // 2. 加载配置并注册 Provider/Model（带缓存，重复调用幂等）
    ensure_registered(&agent_paths)?;

    // 3. 确定 model_id：显式指定 > 配置文件默认 > 第一个已注册模型
    let model_id = agent_ctx
        .model_config
        .model_id
        .clone()
        .or_else(|| get_default_model_id(&agent_paths))
        .ok_or_else(|| {
            let loaded = list_models(&agent_paths);
            InitError::NoModelWithDetail {
                model_id: agent_ctx.model_config.model_id.clone(),
                available: loaded.keys().cloned().collect(),
            }
        })?;

    // 更新 agent_ctx 中的 model_id（确保已设置，供 Engine 内部读取）
    let mut agent_ctx = agent_ctx;
    agent_ctx.model_config.model_id = Some(model_id.clone());

    // 4. 创建 Provider 实例（按 model_id 中 `/` 之前的 provider_id）
    let provider_id = model_id.split('/').next().unwrap_or("");
    let provider = OpenAIProvider::new(provider_id, &agent_paths).ok_or_else(|| {
        let loaded = list_models(&agent_paths);
        InitError::ProviderNotFoundWithDetail {
            provider_id: provider_id.to_string(),
            available_providers: loaded.keys().cloned().collect(),
        }
    })?;

    // 5. 验证模型存在于注册表（防止 provider_id 存在但 model_id 拼写错误）
    if get_model(&model_id, &agent_paths).is_none() {
        return Err(InitError::ModelInfoFailed(model_id));
    }

    // 6. 创建 Engine，返回 (Engine, EngineHandle)
    Ok(Engine::new(Box::new(provider), agent_ctx))
}

/// 确保指定 `agent_paths` 的 Provider/Model 已注册
///
/// 带幂等缓存：先查注册表，非空则直接返回；否则从三层配置加载并注册。
/// 重复调用不会重复加载配置。
fn ensure_registered(agent_paths: &AgentPaths) -> Result<(), InitError> {
    let existing = list_models(agent_paths);
    if !existing.is_empty() {
        return Ok(());
    }

    let config = load_config(agent_paths).map_err(|e| InitError::ConfigError(e.to_string()))?;

    let Some(config) = config else {
        return Ok(());
    };

    // 按 agent_paths 维度注册，支持同进程内多个 Agent 各自独立配置
    let cache_key = agent_paths_cache_key(agent_paths);
    for (provider_id, provider) in &config.providers {
        register_provider(provider_id, provider.clone(), &cache_key);
        for (model_id, model) in &provider.models {
            let full_id = format!("{provider_id}/{model_id}");
            register_model(&full_id, model.clone(), &cache_key);
        }
    }

    Ok(())
}

/// 获取默认 model_id
///
/// 优先级：配置文件 `model` 字段（若在注册表中） > 已加载模型的第一个。
fn get_default_model_id(agent_paths: &AgentPaths) -> Option<String> {
    let loaded = list_models(agent_paths);
    if loaded.is_empty() {
        return None;
    }

    // 配置文件的 model 字段优先，但需确认已注册（防止配置指向未注册模型）
    if let Ok(Some(config)) = load_config(agent_paths)
        && let Some(ref model_id) = config.model
    {
        let lower = model_id.to_lowercase();
        if loaded.contains_key(&lower) {
            return Some(model_id.clone());
        }
    }

    // 回退：取已加载模型的第一个（HashMap 顺序不固定，但能保证有值）
    loaded.keys().next().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// InitError 各变体的 Display 输出应包含关键排错信息。
    ///
    /// init_engine 本身依赖外部状态（环境变量、配置文件、全局注册表缓存），
    /// 不在此做端到端测试；错误信息的可读性是装配函数最值得守护的契约。
    #[test]
    fn init_error_display_carries_context() {
        let no_model = InitError::NoModelWithDetail {
            model_id: None,
            available: vec!["deepseek/deepseek-v4-flash".to_string()],
        };
        let msg = no_model.to_string();
        assert!(msg.contains("未配置 model_id"), "{msg}");
        assert!(msg.contains("deepseek/deepseek-v4-flash"), "{msg}");

        let provider_err = InitError::ProviderNotFoundWithDetail {
            provider_id: "deepseek".to_string(),
            available_providers: vec![],
        };
        let msg = provider_err.to_string();
        assert!(msg.contains("deepseek"), "{msg}");
        assert!(msg.contains("API Key"), "{msg}");

        let model_err = InitError::ModelInfoFailed("foo/bar".to_string());
        assert!(model_err.to_string().contains("foo/bar"));

        let cfg_err = InitError::ConfigError("io 错误".to_string());
        assert!(cfg_err.to_string().contains("io 错误"));
    }
}
