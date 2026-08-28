//! Prompt 模块
//!
//! 系统提示词分层构建，明确分为「覆盖区 + 补充区」。
//! - `loader`: Agent 定义加载（`agents/{name}.md` + frontmatter 解析；未知名返回 `Ok(None)`，文件存在但损坏返回 `Err`——两种语义可区分）
//! - `registry`: Agent 注册表（扫描 `fuyao-agents/` 文件夹列举 agent_id；元素类型见 fuyao-api `selection`）
//! - `builtin`: 统一内置资产模块（Agent 定义与 skills，编译期嵌入 `builtin/assets/` 下的资产文件）
//! - `skills_index`: Skills 读取门面（文件系统四层优先 + 内置兜底，签名与 fuyao_skills 对应函数一致）
//! - `sections`: 分层 section 构建（含补充指令 `instructions/`、Agent 定义列举、子代理索引）
//! - `builder`: `build_system_prompt()` 组装器
//! - `error`: Agent 定义解析错误（未知名 / mode 与用途不符 / 定义文件损坏）

mod builder;
mod builtin;
mod error;
mod loader;
mod registry;
mod sections;
mod skills_index;

pub use builder::{PromptUsage, build_system_prompt};
pub use error::PromptError;
pub use loader::{
    load_agent_definition, load_agent_definition_from_agent_paths, load_builtin_definition,
};
pub use registry::AgentRegistry;
pub use sections::{list_primary_definitions, list_subagent_definitions, resolve_definition};
pub use skills_index::{list_skills, load_skill, load_skill_file};
