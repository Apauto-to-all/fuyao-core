//! Agent 定义模块：定义的解析、获取与搜索
//!
//! Agent 定义（`agents/{name}.md` + frontmatter）是一项提示词能力，由
//! `AgentConfig.definition` 指定加载哪个定义；与 agent_id（数据隔离单元，
//! 见 `registry`）分属不同维度。
//!
//! 分层组织：
//! - `parser`: 定义文件解析纯函数（frontmatter 提取 + 元数据校验）
//! - `loader`: 按层加载（单文件 / 四层优先级查找 / 内置默认表兜底）
//! - `query`: 列举与解析（按模式过滤的列表查询 + 会话定义统一收口出口）
//!
//! 查找链贯穿全部出口：用户 `agents/{name}.md`（workspace > agent > global >
//! extra 四层优先，同名首现胜）→ 内置默认表（default / explore / executor，
//! 最低优先级）。「未找到」（`Ok(None)`）与「文件损坏」（`Err`，立即上抛不跳层）
//! 两种语义严格区分，绝不静默替换人格。
//!
//! 对外暴露两组能力：
//! - 获取：[`load_agent_definition`]（单文件）、[`load_builtin_definition`]（内置表）、
//!   [`load_agent_definition_from_agent_paths`]（四层 + 内置全链查找）
//! - 搜索：[`list_primary_definitions`]（主代理人格列表）、
//!   [`list_subagent_definitions`]（子代理列表）、[`resolve_definition`]
//!   （会话定义统一收口：加载 + mode 校验 + 未知名附可用列表报错）

mod loader;
mod parser;
mod query;

pub use loader::{
    load_agent_definition, load_agent_definition_from_agent_paths, load_builtin_definition,
};
pub use query::{list_primary_definitions, list_subagent_definitions, resolve_definition};
