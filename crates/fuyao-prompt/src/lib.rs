//! Prompt 模块
//!
//! 系统提示词分层构建，明确分为「覆盖区 + 补充区」。
//! - `definitions`: Agent 定义模块（frontmatter 解析 / 四层加载 / 列举搜索 / 会话定义收口）
//! - `registry`: Agent 注册表（扫描 `fuyao-agents/` 文件夹列举 agent_id；元素类型见 fuyao-api `selection`）
//! - `builtin`: 统一内置资产模块（Agent 定义与 skills，编译期嵌入 `builtin/assets/` 下的资产文件）
//! - `skills`: Agent Skills 协议完整实现（发现 / 加载 / 解析 + 内置兜底），对外暴露三个读取出口
//! - `sections`: 分层 section 构建（含补充指令 `instructions/`、技能索引、子代理索引）
//! - `builder`: `build_system_prompt()` 组装器
//! - `error`: Agent 定义解析错误（未知名 / mode 与用途不符 / 定义文件损坏）

mod builder;
mod builtin;
mod definitions;
mod error;
mod registry;
mod sections;
mod skills;

pub use builder::{PromptUsage, build_system_prompt};
pub use definitions::{
    list_primary_definitions, list_subagent_definitions, load_agent_definition,
    load_agent_definition_from_agent_paths, load_builtin_definition, resolve_definition,
};
pub use error::PromptError;
pub use registry::AgentRegistry;
pub use skills::{SkillsError, list_skills, load_skill, load_skill_file};
