//! 引擎初始化 —— 应用层装配入口（配置 / 日志 / Provider 准备）
//!
//! 负责从 [`EngineParams`](fuyao_api::EngineParams) 出发完成引擎装配前的所有准备：
//! 1. 加载 `.env` 环境变量（三层目录）
//! 2. 加载三层 TOML 配置
//! 3. 校验 `[tools.terminal].shell`（显式名非法 / 二进制定位失败即拒绝启动）
//! 4. 初始化日志（tracing subscriber，按 `[logging]` 配置；guard 随返回值传出）
//! 5. 注册 Provider/Model 到注册表（带缓存，重复调用幂等）
//! 6. 校验配置中的模型引用（`[models.fast]` 悬空引用启动即报错）
//! 7. 批量构造所有已注册 Provider 的实例（`ProviderRegistry::from_registered`）
//!
//! 返回 `(ProviderRegistry, 日志 guard)`，
//! 由 [`crate::start`] 组装进 `Engine::new`。工具注册表
//! （[`crate::build_tool_registry`]）独立准备，与本模块解耦。
//!
//! 典型用法（推荐用 [`crate::start`] 一行启动）：
//! ```ignore
//! use fuyao_api::{AgentPaths, EngineParams};
//!
//! let params = EngineParams { agent_paths: AgentPaths::default() };
//! let fuyao_app::InitResult { provider, log_guard } = fuyao_app::init_engine(&params).await?;
//! ```

use crate::logging::LogGuard;
use fuyao_api::{
    AgentPaths, EngineParams, FuyaoConfig, is_config_set, load_config, load_env, set_config,
};
use fuyao_provider::ProviderRegistry;
use fuyao_provider::{agent_paths_cache_key, register_model, register_provider};
use std::sync::Arc;

/// 初始化错误
///
/// 每个变体携带可用于排错的上下文（可用 Provider 列表等），
/// 错误信息直接面向最终用户，包含明确的修正建议（如检查 API Key 配置）。
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    /// 所有 Provider 实例创建失败（注册表为空或 API Key 都未配置）
    #[error("无可用 Provider 实例：所有 Provider 都创建失败（可能是配置文件或 API Key 都未配置）")]
    NoProviderAvailable,

    /// 配置加载失败（TOML 解析错误、IO 错误等）
    #[error("配置加载失败: {0}")]
    ConfigError(String),

    /// agent_id 非法（格式错误 / workspace 来源缺 workspace 参数）
    #[error("agent_id 非法: {0}")]
    InvalidAgentId(String),

    /// 终端 shell 配置非法（名字不在合法值内 / 对应可执行文件未找到）
    #[error("终端 shell 配置非法: {0}")]
    InvalidTerminalShell(String),

    /// 配置中的模型引用悬空（引用的 provider/model 未注册）
    #[error("模型引用无效: {0}")]
    InvalidModelRef(String),
}

/// 引擎装配准备产物
pub struct InitResult {
    /// Provider 实例注册表（多 Provider 路由，注入 `Engine::new`）
    ///
    /// 启动时把所有已注册 Provider 都建实例装进 registry；每个 session 按
    /// `SessionParams.model_config.model_id` 拆出 provider_id 从 registry 取实例路由。
    pub provider: ProviderRegistry,
    /// 日志 guard：drop 时 flush 文件缓冲，须存活到引擎结束
    pub log_guard: LogGuard,
}

/// 引擎装配准备 —— 应用层装配入口
///
/// 从 [`EngineParams`] 出发，一气呵成完成：环境变量加载 → 配置注册 →
/// 日志初始化 → Provider 实例批量构造，
/// 返回可直接喂给 `Engine::new` 的 `(ProviderRegistry, 日志 guard)`。
///
/// 内部取出 `agent_paths` 字段传给下游路径定位子流程——这些子函数只关心路径，
/// 与引擎级扩展字段无关，传裸 [`AgentPaths`] 即可。
///
/// **Provider 创建容错**：批量构造时单个 Provider 实例失败（如 API Key 未配）只记 WARN 跳过，
/// 其他成功的照常注册——支持渐进配置（部分 provider 配错也能启动引擎）。
/// 但若**所有** Provider 都失败，返回 [`InitError::NoProviderAvailable`]。
///
/// # 参数
/// - `params`：引擎启动参数，内含 Agent 三层目录身份证明，决定配置与数据路径。
///
/// # 错误
/// - [`InitError::InvalidAgentId`]：agent_id 格式非法（裸名 / 未知来源）或
///   workspace 来源缺 workspace 参数
/// - [`InitError::InvalidTerminalShell`]：[tools.terminal].shell 显式名不在合法值内
///   或对应可执行文件未找到
/// - [`InitError::InvalidModelRef`]：[models.fast] 引用的模型未注册
/// - [`InitError::NoProviderAvailable`]：所有 Provider 实例创建失败
/// - [`InitError::ConfigError`]：配置文件加载失败
pub async fn init_engine(params: &EngineParams) -> Result<InitResult, InitError> {
    // 内部工具函数只需要路径身份证明，直接取裸 &AgentPaths 复用
    let agent_paths = &params.agent_paths;

    // 0. agent_id 前置校验：来源前缀必须显式（global/{名} / workspace/{名}，
    //    大小写不敏感），workspace 来源必须配 workspace 参数。启动即报错
    //    （fail-fast），路径方法中的 panic 是校验后的不可达兜底。
    agent_paths.validate().map_err(InitError::InvalidAgentId)?;

    // 1. 加载 .env 环境变量
    load_env(agent_paths);

    // 2. 加载配置（一次）：注入全局只读句柄，供所有模块 get_config 读取；
    //    同时复用于 Provider/Model 注册，避免重复加载。
    //
    //    全局配置是进程级单例（OnceLock）：多 engine 场景下第二次 init_engine 不重复
    //    注入——首个 engine 装配时设的配置全进程共享。检测已设则跳过 set，避免
    //    set_config 的 panic（该 panic 是外部直接重复调用的防线，init_engine 内幂等）。
    let config = load_config(agent_paths).map_err(|e| InitError::ConfigError(e.to_string()))?;
    if let Some(ref cfg) = config {
        if is_config_set() {
            tracing::debug!("全局配置已注入，多 engine 装配跳过重复 set_config");
        } else {
            set_config(Arc::new(cfg.clone()));
        }
    }

    // 3. [tools.terminal].shell 启动校验：显式名必须在合法值内且二进制可定位。
    //    显式配置是用户强意图，非法即拒绝启动（fail loud），不静默换 shell。
    //    经 get_config 读取与 shell 解析（LazyLock）同一份全局配置，校验结果
    //    与运行时实际解析一致；无配置文件时读 default（auto），天然通过。
    fuyao_tools::validate_shell_name(&fuyao_api::get_config().tools.terminal.shell)
        .map_err(InitError::InvalidTerminalShell)?;

    // 4. 初始化日志：配置加载后 subscriber 尽早接管，guard 随返回值传出供 AppContext 持有。
    //    文件层失败时自动降级为纯 stderr，不阻断启动（日志是辅助设施）。
    let logging_config = config
        .as_ref()
        .map(|c| c.logging.clone())
        .unwrap_or_default();
    let log_guard = crate::logging::init_logging(&logging_config, agent_paths);

    // 5. 注册 Provider/Model 配置到注册表（带缓存，重复调用幂等）
    ensure_registered(agent_paths, config.as_ref())?;

    // 6. 模型引用校验闭环：[models.fast] 引用的模型必须存在于已注册目录，
    //    悬空引用启动即报错（错误信息附可用模型列表，指明排查方向）
    validate_model_refs(config.as_ref(), agent_paths)?;

    // 7. 批量构造所有已注册 Provider 的实例（单个失败仅 WARN 跳过）
    let provider = ProviderRegistry::from_registered(agent_paths);
    if provider.is_empty() {
        return Err(InitError::NoProviderAvailable);
    }

    tracing::info!(
        fast_model = ?config.as_ref().and_then(|c| c.models.fast.as_ref().map(|r| &r.model)),
        provider_count = provider.provider_ids().len(),
        providers = ?provider.provider_ids(),
        console = logging_config.console,
        "引擎装配准备完成"
    );
    Ok(InitResult {
        provider,
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
    use fuyao_provider::{list_models, list_providers};

    // 已注册过（providers 非空）则跳过——支持幂等调用
    if !list_providers(agent_paths).is_empty() {
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

    // 记录日志（debug 级）：list_models 不再用于推断，仅用于诊断
    tracing::debug!(
        registered_models = list_models(agent_paths).len(),
        "Provider/Model 配置已注册"
    );

    Ok(())
}

/// 校验配置中的模型引用（`[models.fast]`）指向已注册的模型
///
/// 引用悬空（供应商未定义 / 模型不在其旗下）启动即报错——配置引用与供应商
/// 目录之间的闭环校验，把「配了不生效」的死配置拦在启动期。错误信息附
/// 可用模型列表，直接指明修正方向。
fn validate_model_refs(
    config: Option<&FuyaoConfig>,
    agent_paths: &AgentPaths,
) -> Result<(), InitError> {
    use fuyao_provider::list_models;

    // 未配置引用 = 无可校验项（fast 缺省时轻量任务回退到会话模型，运行期自洽）
    let Some(fast) = config.and_then(|c| c.models.fast.as_ref()) else {
        return Ok(());
    };

    // 注册缓存键已小写归一，引用同样小写比对（与运行期解析的大小写不敏感一致）
    let models = list_models(agent_paths);
    if models.contains_key(&fast.model.to_lowercase()) {
        return Ok(());
    }

    let mut available: Vec<String> = models.keys().cloned().collect();
    available.sort();
    Err(InitError::InvalidModelRef(format!(
        "[models.fast] 引用的模型 \"{}\" 不存在，已注册的模型：{}",
        fast.model,
        if available.is_empty() {
            "（无）".to_string()
        } else {
            available.join(", ")
        }
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// InitError::NoProviderAvailable 的 Display 输出应面向最终用户，含修正建议
    #[test]
    fn init_error_no_provider_display_is_helpful() {
        let err = InitError::NoProviderAvailable;
        let msg = err.to_string();
        assert!(msg.contains("无可用 Provider"), "{msg}");
        assert!(msg.contains("API Key"), "{msg}");
    }

    /// InitError::ConfigError 透传原始错误信息
    #[test]
    fn init_error_config_error_passes_message() {
        let err = InitError::ConfigError("io 错误".to_string());
        assert!(err.to_string().contains("io 错误"));
    }

    /// InitError::InvalidAgentId 透传校验错误信息（含格式建议）
    #[test]
    fn init_error_invalid_agent_id_passes_message() {
        let err =
            InitError::InvalidAgentId("agent_id 格式错误（应为 global/{名}）: coder".to_string());
        let msg = err.to_string();
        assert!(msg.contains("agent_id 非法"), "{msg}");
        assert!(msg.contains("global/{名}"), "{msg}");
    }

    /// InitError::InvalidTerminalShell 透传校验错误信息（含合法值提示）
    #[test]
    fn init_error_invalid_terminal_shell_passes_message() {
        let err = InitError::InvalidTerminalShell(
            "shell = \"zsh\" 不在合法值内（合法值：auto | git_bash | powershell | cmd | bash | sh）".to_string(),
        );
        let msg = err.to_string();
        assert!(msg.contains("终端 shell 配置非法"), "{msg}");
        assert!(msg.contains("zsh"), "{msg}");
        assert!(msg.contains("合法值"), "{msg}");
    }

    /// InitError::InvalidModelRef 透传校验错误信息（含模型名与排查方向）
    #[test]
    fn init_error_invalid_model_ref_passes_message() {
        let err = InitError::InvalidModelRef(
            "[models.fast] 引用的模型 \"deepseek/none\" 不存在，已注册的模型：（无）".to_string(),
        );
        let msg = err.to_string();
        assert!(msg.contains("模型引用无效"), "{msg}");
        assert!(msg.contains("deepseek/none"), "{msg}");
    }

    // ===== validate_model_refs：引用校验闭环 =====

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

    /// 悬空引用：报错且信息含引用名与可用模型列表
    #[test]
    fn validate_model_refs_rejects_dangling_reference() {
        let paths = AgentPaths {
            agent_id: Some("global/init_validate_dangling".to_string()),
            ..Default::default()
        };
        let key = fuyao_provider::agent_paths_cache_key(&paths);
        fuyao_provider::register_model(
            "deepseek/deepseek-v4-flash",
            test_model("deepseek-v4-flash"),
            &key,
        );

        let config = FuyaoConfig {
            models: fuyao_api::ModelSelection {
                fast: Some(fuyao_api::ModelRef {
                    model: "deepseek/none".to_string(),
                    ..Default::default()
                }),
            },
            ..Default::default()
        };
        let err = validate_model_refs(Some(&config), &paths).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("deepseek/none"), "{msg}");
        assert!(msg.contains("deepseek-v4-flash"), "应附可用模型列表：{msg}");

        fuyao_provider::clear_cache(&paths);
    }

    /// 命中引用：通过（大小写不敏感）
    #[test]
    fn validate_model_refs_accepts_registered_reference() {
        let paths = AgentPaths {
            agent_id: Some("global/init_validate_hit".to_string()),
            ..Default::default()
        };
        let key = fuyao_provider::agent_paths_cache_key(&paths);
        fuyao_provider::register_model(
            "deepseek/deepseek-v4-flash",
            test_model("deepseek-v4-flash"),
            &key,
        );

        let config = FuyaoConfig {
            models: fuyao_api::ModelSelection {
                fast: Some(fuyao_api::ModelRef {
                    model: "DeepSeek/DeepSeek-V4-Flash".to_string(),
                    ..Default::default()
                }),
            },
            ..Default::default()
        };
        assert!(validate_model_refs(Some(&config), &paths).is_ok());

        fuyao_provider::clear_cache(&paths);
    }

    /// 未配置 fast / 无配置：无可校验项，直接通过
    #[test]
    fn validate_model_refs_passes_without_reference() {
        let paths = AgentPaths::default();
        assert!(validate_model_refs(None, &paths).is_ok());
        assert!(validate_model_refs(Some(&FuyaoConfig::default()), &paths).is_ok());
    }
}
