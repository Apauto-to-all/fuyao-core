//! Prompt 类型定义
//!
//! Agent 定义等核心类型，从 system.md 文件解析。

/// Agent 定义（从 system.md 文件解析）
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
    }

    #[test]
    fn agent_definition_default() {
        let def = AgentDefinition::default();
        assert_eq!(def.name, "");
        assert_eq!(def.version, "1.0.0");
        assert!(def.source_path.is_none());
    }
}
