//! Prompt 类型定义
//!
//! Agent 定义等核心类型，从 `agents/*.md` 文件解析。

use serde::{Deserialize, Serialize};

/// Agent 定义的使用模式
///
/// 区分主代理与子代理，控制 Agent 定义可用于哪些场景：
/// - [`AgentMode::Primary`]：只能作为主代理（`agent_ctx.definition`）使用
/// - [`AgentMode::Subagent`]：只能由子代理工具派生使用
///
/// 二分设计：一个定义要么是主代理人格，要么是专职子代理，职责互斥、
/// 子代理列表只含专职子代理，不会被主代理人格污染。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AgentMode {
    /// 主代理（只能作为 agent_ctx.definition 使用）
    #[default]
    Primary,
    /// 子代理（只能由子代理工具派生使用）
    Subagent,
}

impl AgentMode {
    /// 是否可用作主代理
    pub fn is_usable_as_primary(&self) -> bool {
        matches!(self, AgentMode::Primary)
    }

    /// 是否可用作子代理
    pub fn is_usable_as_subagent(&self) -> bool {
        matches!(self, AgentMode::Subagent)
    }
}

/// 从 frontmatter 字符串解析模式（未知值回退 Primary）
impl From<&str> for AgentMode {
    fn from(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "primary" => AgentMode::Primary,
            "subagent" => AgentMode::Subagent,
            _ => AgentMode::Primary,
        }
    }
}

/// Agent 定义（从 `agents/*.md` 文件解析）
///
/// 只包含纯身份和提示词信息。
/// 模型和工具配置由 fuyao.toml 三层配置管理。
#[derive(Debug, Clone)]
pub struct AgentDefinition {
    /// Agent 名称，为空时从文件名读取
    pub name: String,
    /// Agent 描述
    pub description: String,
    /// 版本号
    pub version: String,
    /// 作者
    pub author: String,
    /// 使用模式：主代理 / 子代理
    pub mode: AgentMode,
    /// 系统提示词内容
    pub system_prompt: String,
    /// 来源文件路径
    pub source_path: Option<String>,
}

impl AgentDefinition {
    /// 创建 AgentDefinition
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        system_prompt: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            version: "1.0.0".to_string(),
            author: String::new(),
            mode: AgentMode::Primary,
            system_prompt: system_prompt.into(),
            source_path: None,
        }
    }
}

impl Default for AgentDefinition {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            version: "1.0.0".to_string(),
            author: String::new(),
            mode: AgentMode::Primary,
            system_prompt: String::new(),
            source_path: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_definition_new_basic() {
        let def = AgentDefinition::new("test", "desc", "system prompt");
        assert_eq!(def.name, "test");
        assert_eq!(def.description, "desc");
        assert_eq!(def.system_prompt, "system prompt");
        assert_eq!(def.version, "1.0.0");
        assert_eq!(def.mode, AgentMode::Primary);
    }

    #[test]
    fn agent_definition_default() {
        let def = AgentDefinition::default();
        assert_eq!(def.name, "");
        assert_eq!(def.version, "1.0.0");
        assert!(def.source_path.is_none());
        assert_eq!(def.mode, AgentMode::Primary);
    }

    #[test]
    fn agent_mode_default_is_primary() {
        assert_eq!(AgentMode::default(), AgentMode::Primary);
    }

    #[test]
    fn agent_mode_usable_checks() {
        assert!(AgentMode::Primary.is_usable_as_primary());
        assert!(!AgentMode::Primary.is_usable_as_subagent());

        assert!(!AgentMode::Subagent.is_usable_as_primary());
        assert!(AgentMode::Subagent.is_usable_as_subagent());
    }

    #[test]
    fn agent_mode_from_str() {
        assert_eq!(AgentMode::from("primary"), AgentMode::Primary);
        assert_eq!(AgentMode::from("subagent"), AgentMode::Subagent);
        // 未知值/空值默认 Primary
        assert_eq!(AgentMode::from("unknown"), AgentMode::Primary);
        assert_eq!(AgentMode::from(""), AgentMode::Primary);
        // 大小写不敏感
        assert_eq!(AgentMode::from("Primary"), AgentMode::Primary);
        assert_eq!(AgentMode::from(" SUBAGENT "), AgentMode::Subagent);
    }
}
