//! 工具定义类型
//!
//! 遵循 OpenAI Function Calling 规范的工具定义。
//! 包含工具参数属性、参数定义、Schema 和完整定义。

use std::collections::HashMap;

/// 工具参数属性定义
///
/// 定义单个参数的类型和描述信息。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolParameterProperty {
    /// 参数类型：string, integer, number, boolean, array, object
    #[serde(rename = "type")]
    pub kind: String,

    /// 参数描述
    pub description: String,

    /// 默认值
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,

    /// 枚举值列表
    #[serde(rename = "enum", skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<String>>,

    /// 数组元素类型定义
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<HashMap<String, serde_json::Value>>,
}

impl Default for ToolParameterProperty {
    fn default() -> Self {
        Self {
            kind: "string".to_string(),
            description: String::new(),
            default: None,
            enum_values: None,
            items: None,
        }
    }
}

/// 工具参数定义
///
/// 定义工具的所有参数，包括类型、必需参数等。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolParameters {
    /// 参数类型，固定为 object
    #[serde(rename = "type")]
    pub kind: String,

    /// 参数属性映射
    pub properties: HashMap<String, ToolParameterProperty>,

    /// 必需参数列表
    pub required: Vec<String>,
}

impl Default for ToolParameters {
    fn default() -> Self {
        Self {
            kind: "object".to_string(),
            properties: HashMap::new(),
            required: Vec::new(),
        }
    }
}

/// 工具 Schema 定义
///
/// 遵循 OpenAI Function Calling 规范的工具定义。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolSchema {
    /// 工具名称，唯一标识
    pub name: String,

    /// 工具功能描述
    pub description: String,

    /// 参数定义
    pub parameters: ToolParameters,
}

/// 工具完整定义
///
/// OpenAI API 调用时使用的工具定义格式。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolDefinition {
    /// 工具类型，固定为 function
    #[serde(rename = "type")]
    pub kind: String,

    /// 工具函数定义
    pub function: ToolSchema,
}

impl ToolDefinition {
    /// 创建新的工具定义
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            kind: "function".to_string(),
            function: ToolSchema {
                name: name.into(),
                description: description.into(),
                parameters: ToolParameters::default(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definition_new_creates_function_tool() {
        let tool = ToolDefinition::new("read_file", "读取文件内容");
        assert_eq!(tool.kind, "function");
        assert_eq!(tool.function.name, "read_file");
        assert_eq!(tool.function.description, "读取文件内容");
    }

    #[test]
    fn tool_parameters_default_is_object() {
        let params = ToolParameters::default();
        assert_eq!(params.kind, "object");
        assert!(params.properties.is_empty());
        assert!(params.required.is_empty());
    }

    #[test]
    fn tool_parameter_property_default_is_string() {
        let prop = ToolParameterProperty::default();
        assert_eq!(prop.kind, "string");
        assert!(prop.description.is_empty());
    }
}
