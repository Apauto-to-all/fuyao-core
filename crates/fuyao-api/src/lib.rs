//! Fuyao API 类型定义
//!
//! 公共 trait + 类型定义，无业务逻辑依赖。
//! 按领域组织模块：
//! - `agent`: Agent 运行上下文、路径配置
//! - `config`: 全局配置类型与共享句柄（单一真相源）
//! - `message`: 消息类型（输入/输出）
//! - `paths`: 路径系统（三层目录架构）
//! - `provider`: 模型和供应商配置
//! - `tool`: 工具定义和执行

mod agent;
mod config;
mod control;
mod error;
mod mcp_types;
pub mod message;
mod paths;
mod prompt_types;
mod provider;
mod queue_snapshot;
mod session_types;
mod skill_types;
mod tool;

// 导出常用类型
pub use agent::{AgentConfig, AgentPaths, EngineParams, ModelConfig, SessionParams};
pub use config::{
    CompressionConfig, ConfigError, EngineConfig, FuyaoConfig, GuardConfig, HooksConfig,
    ImageConfig, LlmConfig, LogRotation, LoggingConfig, LoopGuardConfig, McpGlobalConfig, ModelRef,
    ModelSelection, PluginsConfig, RetryConfig, SessionConfig, SessionStorageConfig, TitleConfig,
    ToolRunnerConfig, ToolsConfig, ToolsLimitsConfig, get_config, is_config_set, load_config,
    load_env, load_merged_config, set_config, unknown_tool_names,
};
pub use control::{ControlCommand, TurnDirective};
pub use error::ApiError;
pub use mcp_types::MCPServerConfig;
pub use message::{
    EventBase, InputEvent, InterruptSource, OutputEvent, PluginEventSource, PluginSource,
    SystemSource, UserMessageMode, UserMessageSource,
};
pub use paths::{
    LayeredPaths, get_agent_root, get_fuyao_agents_dir, get_fuyao_home, get_workspace_agents_dir,
    get_workspace_root,
};
pub use prompt_types::{AgentDefinition, AgentMode};
pub use provider::{
    InputModality, Model, ModelCost, ModelLimit, ModelModalities, OutputModality, PriceTier,
    Provider, ProviderOptions, ThinkingType,
};
pub use queue_snapshot::{QueueSnapshot, QueueSnapshotItem};
pub use session_types::{
    ImageContent, Message, MessageKind, MessageRole, Session, TodoItem, normalize_workspace,
};
pub use skill_types::{LINKED_SUBDIRS, SkillDefinition, SkillMeta};
pub use tool::{
    CancellationToken, ChildSessionSource, SubagentOps, ToolCallContext, ToolDefinition, ToolFn,
    ToolParameterProperty, ToolParameters, ToolResult, ToolSchema,
};
