//! 引擎初始化 —— 应用层装配入口（配置 / 日志 / Provider 准备）
//!
//! 负责从 [`AgentPaths`] 出发完成引擎装配前的所有准备：
//! 1. 加载 `.env` 环境变量（三层目录）
//! 2. 加载三层 TOML 配置
//! 3. 初始化日志（tracing subscriber，按 `[logging]` 配置；guard 随返回值传出）
//! 4. 注册 Provider/Model 到注册表（带缓存，重复调用幂等）
//! 5. 确定 `model_id`（配置文件 `[models.default]` > 第一个已注册模型）
//! 6. 创建 [`OpenAIProvider`] 实例（按 `provider_id`）
//! 7. 验证模型存在于注册表
//!
//! 返回 `(provider, 默认 model_id, 日志 guard)`，由 [`crate::start`] 组装进 `Engine::new`。
//! 工具注册表（[`crate::build_tool_registry`]）独立准备，与本模块解耦。
//!
//! 典型用法（推荐用 [`crate::start`] 一行启动）：
//! ```ignore
//! use fuyao_api::AgentPaths;
//!
//! let agent_paths = AgentPaths::default();
//! let (provider, model_id, _log_guard) = fuyao_app::init_engine(agent_paths).await?;
//! ```

use crate::logging::LogGuard;
use fuyao_api::{AgentPaths, FuyaoConfig, load_config, load_env, set_config};
use fuyao_provider::OpenAIProvider;
use fuyao_provider::{
    Provider, agent_paths_cache_key, get_model, list_models, register_model, register_provider,
};
use std::sync::Arc;

/// 初始化错误
///
/// 每个变体携带可用于排错的上下文（可用模型/Provider 列表等），
/// 错误信息直接面向最终用户，包含明确的修正建议（如检查 API Key 配置）。
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    /// 注册表中没有任何可用模型（无法确定 provider_id）
    #[error("无可用模型 (可用: {available:?})。请检查配置文件 [providers] 段")]
    NoModelWithDetail {
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

/// 引擎装配准备产物
pub struct InitResult {
    /// 已构造的 LLM Provider（引擎级共享，注入 `Engine::new`）
    pub provider: Arc<dyn Provider>,
    /// 推断出的默认 model_id（消息级缺省时兜底用，调用方发消息时可用此填充 MessageParams）
    pub default_model_id: String,
    /// 日志 guard：drop 时 flush 文件缓冲，须存活到引擎结束
    pub log_guard: LogGuard,
}

/// 引擎装配准备 —— 应用层装配入口
///
/// 从 [`AgentPaths`] 出发，一气呵成完成：环境变量加载 → 配置注册 →
/// 日志初始化 → 默认 model_id 确定 → Provider 创建 → 模型校验，
/// 返回可直接喂给 `Engine::new` 的 `(provider, 默认 model_id, 日志 guard)`。
///
/// 注意：model_id 现在是消息级属性（跟每条消息走），但 Provider 是引擎级共享。
/// 本函数确定一个默认 model_id 仅用于挑 provider_id 创建 Provider 实例；
/// 调用方发消息时仍可在 [`fuyao_api::MessageParams`] 里自由指定每轮模型。
///
/// # 参数
/// - `agent_paths`：Agent 三层目录身份证明，决定配置与数据路径。
///
/// # 错误
/// - [`InitError::NoModelWithDetail`]：注册表为空，无法确定 provider_id
/// - [`InitError::ProviderNotFoundWithDetail`]：Provider 创建失败（API Key 缺失等）
/// - [`InitError::ModelInfoFailed`]：推断的 model_id 在注册表中不存在
/// - [`InitError::ConfigError`]：配置文件加载失败
pub async fn init_engine(agent_paths: AgentPaths) -> Result<InitResult, InitError> {
    // 1. 加载 .env 环境变量
    load_env(&agent_paths);

    // 2. 加载配置（一次）：注入全局只读句柄，供所有模块 get_config 读取；
    //    同时复用于 Provider/Model 注册与默认 model_id 推断，避免重复加载。
    let config = load_config(&agent_paths).map_err(|e| InitError::ConfigError(e.to_string()))?;
    if let Some(ref cfg) = config {
        set_config(Arc::new(cfg.clone()));
    }

    // 3. 初始化日志：配置加载后 subscriber 尽早接管，guard 随返回值传出供 AppContext 持有。
    //    文件层失败时自动降级为纯 stderr，不阻断启动（日志是辅助设施）。
    let logging_config = config
        .as_ref()
        .map(|c| c.logging.clone())
        .unwrap_or_default();
    let log_guard = crate::logging::init_logging(&logging_config, &agent_paths);

    // 4. 注册 Provider/Model（带缓存，重复调用幂等）
    ensure_registered(&agent_paths, config.as_ref())?;

    // 5. 确定 model_id：配置文件默认 > 第一个已注册模型
    let model_id = get_default_model_id(&agent_paths, config.as_ref()).ok_or_else(|| {
        let loaded = list_models(&agent_paths);
        InitError::NoModelWithDetail {
            available: loaded.keys().cloned().collect(),
        }
    })?;

    // 6. 创建 Provider 实例（按 model_id 中 `/` 之前的 provider_id）
    let provider_id = model_id.split('/').next().unwrap_or("");
    let provider = OpenAIProvider::new(provider_id, &agent_paths).ok_or_else(|| {
        let loaded = list_models(&agent_paths);
        InitError::ProviderNotFoundWithDetail {
            provider_id: provider_id.to_string(),
            available_providers: loaded.keys().cloned().collect(),
        }
    })?;

    // 7. 验证模型存在于注册表（防止 provider_id 存在但 model_id 拼写错误）
    if get_model(&model_id, &agent_paths).is_none() {
        return Err(InitError::ModelInfoFailed(model_id));
    }

    let provider: Arc<dyn Provider> = Arc::new(provider);

    tracing::info!(
        model_id = %model_id,
        console = logging_config.console,
        "引擎装配准备完成"
    );
    Ok(InitResult {
        provider,
        default_model_id: model_id,
        log_guard,
    })
}

/// 确保指定 `agent_paths` 的 Provider/Model 已注册
///
/// 带幂等缓存：先查注册表，非空则直接返回；否则用 init_engine 已加载的配置注册。
/// 不再重复调用 load_config（由调用方一次性加载后传入）。
fn ensure_registered(
    agent_paths: &AgentPaths,
    config: Option<&FuyaoConfig>,
) -> Result<(), InitError> {
    let existing = list_models(agent_paths);
    if !existing.is_empty() {
        return Ok(());
    }

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
/// 优先级：配置文件 `[models.default]`（若在注册表中） > 已加载模型的第一个。
/// 使用 init_engine 已加载的配置，不重复加载。
fn get_default_model_id(agent_paths: &AgentPaths, config: Option<&FuyaoConfig>) -> Option<String> {
    let loaded = list_models(agent_paths);
    if loaded.is_empty() {
        return None;
    }

    // 配置文件的 [models.default] 优先，但需确认已注册（防止配置指向未注册模型）
    if let Some(cfg) = config
        && let Some(model_ref) = cfg.models.default.as_ref()
    {
        let lower = model_ref.model.to_lowercase();
        if loaded.contains_key(&lower) {
            return Some(model_ref.model.clone());
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
            available: vec!["deepseek/deepseek-v4-flash".to_string()],
        };
        let msg = no_model.to_string();
        assert!(msg.contains("无可用模型"), "{msg}");
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
