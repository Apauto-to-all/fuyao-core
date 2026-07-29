//! Prompt 模块
//!
//! 系统提示词分层构建，明确分为「覆盖区 + 补充区」。
//! - `loader`: Agent 定义加载（`agents/default.md` + frontmatter 解析 + 内置默认回退）
//! - `registry`: Agent 注册表（扫描目录、列举/查询 Agent）
//! - `default`: 内置默认 Agent 定义（编译期嵌入 `defaults/` 下的 .md 文件）
//! - `sections`: 分层 section 构建（含补充指令 `instructions/`、子代理索引）
//! - `builder`: `build_system_prompt()` 组装器

mod builder;
mod default;
mod error;
mod loader;
mod registry;
mod sections;

pub use builder::{PromptUsage, build_system_prompt};
pub use error::PromptError;
pub use loader::{
    load_agent_definition, load_agent_definition_from_agent_paths, load_builtin_definition,
};
pub use registry::{
    AgentContent, AgentFile, AgentInfo, AgentRegistry, AgentSource, PagedAgents, RegistryError,
    UpdateContentRequest,
};
pub use sections::{list_subagent_definitions, resolve_definition};
