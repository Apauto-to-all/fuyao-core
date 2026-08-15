//! 工具执行结果信封
//!
//! 工具产出形态的统一出口：JSON 对象结果、纯文本结果、错误三变体。
//! wire 序列化（[`ToolOutput::to_wire`]）由本模块单点拥有——
//! `{"error": ...}` 键约定在此类型化，调用方（如 MCP 熔断器）以
//! [`ToolOutput::is_error`] 判定成败，不再解析结果字符串。

use serde_json::{Map, Value};

/// 工具执行错误
///
/// `message` 恒映射 wire 上的 `"error"` 键；`extras` 携带修正建议
/// （`suggestion`）、相关路径（`path`）等附加字段，帮助 LLM 自行修正后重试。
#[derive(Debug, Clone)]
pub struct ToolError {
    /// 错误主信息（wire 上恒为 `"error"` 键）
    pub message: String,

    /// 附加字段（如 `suggestion` / `path`），序列化时平铺进错误对象
    pub extras: Map<String, Value>,
}

impl ToolError {
    /// 创建错误（仅主信息）
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            extras: Map::new(),
        }
    }

    /// 追加一个附加字段（链式）
    ///
    /// 常用于补修正建议与相关路径：`ToolError::new(msg).with("suggestion", "...")`
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.extras.insert(key.to_string(), value.into());
        self
    }

    /// 序列化为 wire 形态：`{"error": message, ...extras}`
    pub fn to_json(&self) -> Value {
        let mut obj = Map::with_capacity(1 + self.extras.len());
        obj.insert("error".to_string(), Value::String(self.message.clone()));
        obj.extend(self.extras.clone());
        Value::Object(obj)
    }
}

impl From<String> for ToolError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for ToolError {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

/// 工具执行结果信封
///
/// 三种产出形态的单一出口，wire 形态与序列化规则如下：
/// - [`Value`](ToolOutput::Value)：JSON 对象结果（多数工具），wire = 对象序列化原文
/// - [`Text`](ToolOutput::Text)：纯文本结果（子代理最终回复等），wire = 原文，不裹引号
/// - [`Err`](ToolOutput::Err)：错误，wire = `{"error": message, ...extras}`
#[derive(Debug, Clone)]
pub enum ToolOutput {
    /// JSON 对象结果（多数工具）
    Value(Value),

    /// 纯文本结果（保持原文回喂，不裹 JSON 引号）
    Text(String),

    /// 错误（wire 上恒有 `"error"` 键）
    Err(ToolError),
}

impl ToolOutput {
    /// 构造 JSON 对象结果
    pub fn ok(value: Value) -> Self {
        Self::Value(value)
    }

    /// 构造纯文本结果
    pub fn text(content: impl Into<String>) -> Self {
        Self::Text(content.into())
    }

    /// 构造错误结果
    pub fn error(message: impl Into<String>) -> Self {
        Self::Err(ToolError::new(message))
    }

    /// 是否为错误结果（熔断 / 日志据此判定成败，替代字符串解析）
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Err(_))
    }

    /// 序列化为回喂 LLM 的 wire 字符串
    pub fn to_wire(&self) -> String {
        match self {
            Self::Value(v) => v.to_string(),
            Self::Text(s) => s.clone(),
            Self::Err(e) => e.to_json().to_string(),
        }
    }
}

impl From<ToolError> for ToolOutput {
    fn from(e: ToolError) -> Self {
        Self::Err(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_error_new_only_message() {
        let e = ToolError::new("文件不存在");
        assert_eq!(e.message, "文件不存在");
        assert!(e.extras.is_empty());
    }

    #[test]
    fn tool_error_with_appends_extras() {
        let e = ToolError::new("路径参数不能为空")
            .with("suggestion", "请提供有效的文件路径")
            .with("path", "/tmp/x");
        assert_eq!(e.extras.len(), 2);
        assert_eq!(e.extras["suggestion"], json!("请提供有效的文件路径"));
    }

    #[test]
    fn tool_error_to_json_error_key_first_class() {
        let e = ToolError::new("写入失败").with("suggestion", "检查权限");
        let v = e.to_json();
        assert_eq!(v["error"], json!("写入失败"));
        assert_eq!(v["suggestion"], json!("检查权限"));
    }

    #[test]
    fn from_str_and_string_build_error() {
        let a: ToolError = "裸串".into();
        let b: ToolError = String::from("String").into();
        assert_eq!(a.message, "裸串");
        assert_eq!(b.message, "String");
    }

    #[test]
    fn value_variant_serializes_object_verbatim() {
        let out = ToolOutput::ok(json!({"result": "内容", "total": 3}));
        assert_eq!(out.to_wire(), r#"{"result":"内容","total":3}"#);
    }

    #[test]
    fn text_variant_keeps_raw_text() {
        let out = ToolOutput::text("最终回复");
        assert_eq!(out.to_wire(), "最终回复");
        assert!(!out.is_error());
    }

    #[test]
    fn err_variant_serializes_error_json() {
        let out = ToolOutput::error("超时");
        assert!(out.is_error());
        assert_eq!(out.to_wire(), r#"{"error":"超时"}"#);
    }

    #[test]
    fn tool_error_converts_into_output() {
        let out: ToolOutput = ToolError::new("失败").into();
        assert!(out.is_error());
    }
}
