//! Prompt 模块
//!
//! 系统提示词分层构建，明确分为「覆盖区 + 补充区」。
//! - `loader`: Agent 定义加载（`agents/default.md` + frontmatter 解析）
//! - `registry`: Agent 注册表（扫描目录、列举/查询 Agent）
//! - `default`: 默认 Agent 定义（`agents/default.md` 不存在时使用）
//! - `sections`: 分层 section 构建（含补充指令 `instructions/`）
//! - `builder`: `build_system_prompt()` 组装器

mod builder;
mod default;
mod error;
mod loader;
mod registry;
mod sections;

pub use builder::build_system_prompt;
pub use error::PromptError;
pub use loader::{load_agent_definition, load_agent_definition_from_agent_paths};
pub use registry::{
    AgentContent, AgentFile, AgentInfo, AgentRegistry, AgentSource, PagedAgents, RegistryError,
    UpdateContentRequest,
};
