//! Agent 注册表的对外数据类型与错误
//!
//! 与扫描/读写逻辑分离：这里集中描述「列表 / 详情 / 编辑请求」的 wire 形态，
//! 以及注册表操作可能返回的错误，供 registry 主模块与消费方共用。

use fuyao_api::AgentMode;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Agent 来源层
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum AgentSource {
    /// 全局层：~/.fuyao/fuyao-agents/
    Global,
    /// 项目层：{workspace}/.fuyao/fuyao-agents/
    Workspace,
}

/// Agent 完整信息（列表与详情共用）
///
/// 合并原 AgentInfo（轻量）与 AgentDetail（完整）为单一结构。
/// Rust 解析 md/toml 足够快，列表分页后单次返回量可控，
/// 前端点击卡片展开可直接复用列表数据，无需详情接口。
#[derive(Debug, Clone, Serialize)]
pub struct AgentInfo {
    /// Agent 标识（带来源前缀）：`global/{文件夹名}` / `workspace/{文件夹名}` / `default`
    pub id: String,
    /// 显示名（system.md frontmatter）
    pub name: String,
    /// 能力描述（system.md frontmatter）
    pub description: String,
    /// 使用模式：主代理 / 子代理 / 全部（system.md frontmatter mode 字段）
    pub mode: AgentMode,
    /// 来源层
    pub source: AgentSource,
    /// 完整系统提示词（system.md body）
    pub system_prompt: String,
    /// 模型配置（fuyao.toml model 字段）
    pub model: Option<String>,
    /// 工具配置（fuyao.toml `[tools]` 表的 key 列表）
    pub tools: Vec<String>,
    /// MCP 服务器引用（fuyao.toml `[mcp_servers]` 表的 key 列表）
    pub mcp_servers: Vec<String>,
    /// Profile 列表（profiles/*.md 文件名，无 .md 后缀）
    pub profiles: Vec<String>,
}

/// 分页响应
#[derive(Debug, Clone, Serialize)]
pub struct PagedAgents {
    /// 当前页的 Agent 列表
    pub items: Vec<AgentInfo>,
    /// Agent 总数（过滤后）
    pub total: usize,
    /// 当前页码（从 1 开始）
    pub page: usize,
    /// 每页大小
    pub size: usize,
}

/// Agent 文件内容读取响应（system.md + fuyao.toml 文本）
#[derive(Debug, Clone, Serialize)]
pub struct AgentContent {
    /// system.md 文本，不存在为空串
    pub system_md: String,
    /// fuyao.toml 文本，不存在为空串
    pub fuyao_toml: String,
}

/// 编辑的目标文件
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentFile {
    /// system.md（角色定义）
    SystemMd,
    /// fuyao.toml（能力配置）
    FuyaoToml,
}

impl AgentFile {
    /// 返回对应的物理文件名
    pub fn file_name(self) -> &'static str {
        match self {
            AgentFile::SystemMd => "system.md",
            AgentFile::FuyaoToml => "fuyao.toml",
        }
    }
}

/// 文件编辑请求（单文件保存）
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateContentRequest {
    /// 编辑的目标文件
    pub file: AgentFile,
    /// 新的文件内容（原样覆盖写入）
    pub content: String,
}

/// Agent 注册表错误
#[derive(Debug, Error)]
pub enum RegistryError {
    /// Agent 名称非法（含路径穿越 / 系统非法字符 / 保留名等）
    #[error("Agent 名称非法：{0}")]
    InvalidName(String),
    /// Agent 文件夹不存在
    #[error("Agent 不存在：{0}")]
    NotFound(String),
    /// Agent 已存在（创建时同名冲突）
    #[error("Agent 已存在：{0}")]
    AlreadyExists(String),
    /// 默认 Agent 禁止编辑或删除
    #[error("默认 Agent 禁止编辑或删除")]
    DefaultForbidden,
    /// 未配置工作目录，无法操作项目层 Agent
    #[error("未配置工作目录，无法操作项目层 Agent")]
    WorkspaceMissing,
    /// 文件读写失败
    #[error("文件操作失败：{0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_source_serializes_pascal_case() {
        assert_eq!(
            serde_json::to_string(&AgentSource::Global).unwrap(),
            "\"Global\""
        );
        assert_eq!(
            serde_json::to_string(&AgentSource::Workspace).unwrap(),
            "\"Workspace\""
        );
    }

    #[test]
    fn agent_file_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&AgentFile::SystemMd).unwrap(),
            "\"system_md\""
        );
        assert_eq!(
            serde_json::to_string(&AgentFile::FuyaoToml).unwrap(),
            "\"fuyao_toml\""
        );
        // 反序列化（PUT 请求体 file 字段）
        let req: UpdateContentRequest =
            serde_json::from_str(r#"{"file":"system_md","content":"x"}"#).unwrap();
        assert_eq!(req.file, AgentFile::SystemMd);
        assert_eq!(req.content, "x");
    }
}
