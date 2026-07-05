//! Fuyao API 类型定义
//!
//! 公共 trait + 类型定义，无业务逻辑依赖。
//! 按领域组织模块：
//! - `agent`: Agent 运行上下文、路径配置
//! - `message`: 消息类型（输入/输出）
//! - `paths`: 路径系统（三层目录架构）
//! - `provider`: 模型和供应商配置
//! - `tool`: 工具定义和执行

pub mod agent;
pub mod error;
pub mod mcp_types;
pub mod message;
pub mod paths;
pub mod plugin_types;
pub mod prompt_types;
pub mod provider;
pub mod queue_snapshot;
pub mod session_types;
pub mod skill_types;
pub mod tool;

// 导出常用类型
pub use agent::{AgentContext, AgentPaths, ModelConfig, SharedAgentCtx, ToolRunnerConfig};
pub use error::ApiError;
pub use mcp_types::MCPServerConfig;
pub use message::{
    EventBase, InputEvent, InterruptSource, OutputEvent, PluginEventSource, PluginOrigin,
    PluginSource, QueueUpdateKind, SystemSource, UserMessageMode, UserMessageSource,
};
pub use paths::parallel::{
    canonicalize_path, extract_path_from_args, paths_overlap, should_parallelize,
};
pub use paths::{
    LayeredPaths, get_agent_root, get_fuyao_agents_dir, get_fuyao_home, get_workspace_agents_dir,
    get_workspace_root,
};
pub use plugin_types::{InterceptPoint, ObservePoint, PluginManifest};
pub use prompt_types::AgentDefinition;
pub use provider::{
    Model, ModelCost, ModelLimit, ModelModalities, PriceTier, Provider, ProviderOptions,
    ThinkingType,
};
pub use queue_snapshot::{QueueSnapshot, QueueSnapshotItem};
pub use session_types::{Message, Session, TodoItem};
pub use skill_types::{LINKED_SUBDIRS, SkillDefinition, SkillMeta};
pub use tool::{
    ToolCallContext, ToolDefinition, ToolFn, ToolParameterProperty, ToolParameters, ToolResult,
    ToolSchema,
};
