//! Fuyao 配置系统 —— 单一真相源
//!
//! 本模块是整个配置系统的唯一实现入口，集中管理：
//! - **类型定义**：所有 `*Config` 子段（按域分文件：`llm`、`guard`、`session`、`mcp`、
//!   `tools`、`engine`、`logging`）
//! - **聚合结构**：`FuyaoConfig`，对应 `fuyao.toml` 顶层
//! - **加载逻辑**：三层合并加载（`loader`）、Provider 容错解析（`providers`）、
//!   环境变量加载（`env`）
//! - **全局只读句柄**：`set_config` / `get_config`
//!
//! ## 选址理由
//!
//! `fuyao-api` 是所有 crate 的公共依赖（自身零内部依赖），把配置入口放这里零循环、
//! 零反向依赖，任何层的模块都能访问。types + loader + handle 同处一个模块树，
//! 形成「一套代码」的单一真相源，便于后续扩展。
//!
//! ## 访问模式
//!
//! 配置加载一次后包成 `Arc<FuyaoConfig>` 存进全局 `OnceLock`，长期只读存在。
//! 任何模块通过 `get_config()` 拿到 `Arc` clone 读取自己那段，一行接入、零参数传播。

pub mod engine;
pub mod env;
pub mod error;
pub mod guard;
pub mod hooks;
pub mod llm;
pub mod loader;
pub mod logging;
pub mod mcp;
pub mod plugins;
pub mod providers;
pub mod session;
pub mod tools;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use serde::Deserialize;

use crate::mcp_types::MCPServerConfig;
use crate::provider::Provider;

// 聚合用子段类型（每域独立文件）
pub use engine::EngineConfig;
pub use guard::{GuardConfig, LoopGuardConfig};
pub use hooks::HooksConfig;
pub use llm::{LlmConfig, RetryConfig};
pub use logging::{LogRotation, LoggingConfig};
pub use mcp::McpGlobalConfig;
pub use plugins::PluginsConfig;
pub use session::{CompressionConfig, SessionConfig, SessionStorageConfig};
pub use tools::{ToolRunnerConfig, ToolsConfig, ToolsLimitsConfig};

// 加载相关
pub use env::load_env;
pub use error::ConfigError;
pub use loader::{load_config, load_merged_config};
pub use providers::load_providers;

/// Fuyao 全局配置聚合
///
/// 对应 `fuyao.toml` 顶层结构。所有子段 `#[serde(default)]`，缺失时走各自 `Default`，
/// 与原硬编码值一致。
///
/// `providers` 字段 `#[serde(skip)]`：容错解析由 `providers::load_providers` 完成
/// （serde 不支持 TOML 整数→f64 价格自动转换），由加载流程单独回填。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct FuyaoConfig {
    /// 默认模型（格式：provider_id/model_id）
    pub model: Option<String>,

    /// Provider 配置字典，key 为 Provider ID
    #[serde(skip)]
    pub providers: HashMap<String, Provider>,

    /// MCP Server 配置字典，key 为 server 名称
    pub mcp_servers: HashMap<String, MCPServerConfig>,

    /// 工具系统聚合（开关 + 运行器 + 限制）
    pub tools: ToolsConfig,

    /// Guard 聚合（循环检测）
    pub guard: GuardConfig,

    /// LLM 调用层（超时 / 退避）
    pub llm: LlmConfig,

    /// 会话（压缩 / 存储）
    pub session: SessionConfig,

    /// MCP 全局 fallback
    pub mcp: McpGlobalConfig,

    /// 引擎通道容量
    pub engine: EngineConfig,

    /// Hooks 配置（超时等）
    pub hooks: HooksConfig,

    /// Plugins 配置（插件开关）
    pub plugins: PluginsConfig,

    /// 日志（级别 / stderr 开关 / 轮转），由 `fuyao-app::init_logging` 消费
    pub logging: LoggingConfig,
}

// ==================== 全局只读句柄 ====================

/// 全局配置存储：加载一次后存入，之后只读
static CONFIG: OnceLock<Arc<FuyaoConfig>> = OnceLock::new();

/// 注入全局配置（仅应用装配启动早期调用一次，如 `fuyao_app::init_engine`）
///
/// 重复调用视为编程错误，直接 panic：配置应只加载一次，重复 set 说明初始化流程出错。
pub fn set_config(config: Arc<FuyaoConfig>) {
    CONFIG
        .set(config)
        .expect("set_config 重复调用：配置只允许注入一次，重复 set 是编程错误");
}

/// 读取全局配置
///
/// - 已 `set_config`：返回所设 `Arc` 的 clone（一次原子引用计数自增，廉价）
/// - 未 set：返回 `Arc::new(FuyaoConfig::default())`，**绝不 panic**
///
/// 未 set 返回 default 是关键兜底：规避初始化时序风险与并行测试串扰
/// （不 set 的测试全部读 default、天然隔离）。
pub fn get_config() -> Arc<FuyaoConfig> {
    CONFIG
        .get()
        .cloned()
        .unwrap_or_else(|| Arc::new(FuyaoConfig::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FuyaoConfig::default() 各子段等于对应 *Config::default()
    #[test]
    fn fuyao_config_default_subsections_match_their_defaults() {
        let c = FuyaoConfig::default();
        assert!(c.model.is_none());
        assert!(c.providers.is_empty());
        assert!(c.mcp_servers.is_empty());
        assert!(c.tools.enabled.is_empty());
        assert_eq!(c.tools.limits.terminal_max_timeout_secs, 6000);
        assert_eq!(c.guard.loop_.tool_repeat_threshold, 4);
        assert_eq!(c.llm.request_timeout_secs, 300);
        assert_eq!(c.session.compression.threshold, 0.85);
        assert_eq!(c.mcp.tool_timeout_secs, 120);
        assert_eq!(c.engine.output_channel_capacity, 256);
        assert_eq!(c.hooks.timeout_secs, 5);
        assert!(c.plugins.enabled.is_empty());
        assert_eq!(c.logging.level, "info");
        assert!(c.logging.console);
        assert_eq!(c.logging.rotation, LogRotation::Daily);
    }

    /// get_config 未 set 时返回 default 不 panic。
    ///
    /// 本单元测试二进制内不调用 set_config（set 发生在独立的集成测试进程），
    /// 故 get_config 必返回 default。
    #[test]
    fn get_config_without_set_returns_default_no_panic() {
        let c = get_config();
        assert_eq!(c.llm.request_timeout_secs, 300);
        assert_eq!(c.engine.output_channel_capacity, 256);
        assert!(c.providers.is_empty());
        assert_eq!(c.logging.level, "info");
    }
}
