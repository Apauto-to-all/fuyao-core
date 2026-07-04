//! 工具执行结果类型
//!
//! 统一的工具执行结果格式，用于返回工具执行的成功/失败状态和内容。

/// 工具执行结果
///
/// 统一的工具执行结果格式。
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// 执行是否成功
    pub success: bool,

    /// 返回内容
    pub content: String,

    /// 错误信息
    pub error: Option<String>,
}

impl Default for ToolResult {
    fn default() -> Self {
        Self {
            success: true,
            content: String::new(),
            error: None,
        }
    }
}

impl ToolResult {
    /// 创建成功的工具执行结果
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            success: true,
            content: content.into(),
            error: None,
        }
    }

    /// 创建失败的工具执行结果
    pub fn error(error: impl Into<String>) -> Self {
        Self {
            success: false,
            content: String::new(),
            error: Some(error.into()),
        }
    }

    /// 转换为 JSON 字符串格式
    pub fn to_json(&self) -> String {
        if self.success {
            serde_json::json!({ "result": &self.content }).to_string()
        } else {
            serde_json::json!({ "error": &self.error }).to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_result_success_creates_success_result() {
        let result = ToolResult::success("文件内容");
        assert!(result.success);
        assert_eq!(result.content, "文件内容");
        assert!(result.error.is_none());
    }

    #[test]
    fn tool_result_error_creates_error_result() {
        let result = ToolResult::error("文件不存在");
        assert!(!result.success);
        assert!(result.content.is_empty());
        assert_eq!(result.error, Some("文件不存在".to_string()));
    }

    #[test]
    fn tool_result_to_json_success_format() {
        let result = ToolResult::success("测试内容");
        let json = result.to_json();
        assert!(json.contains("\"result\""));
        assert!(json.contains("测试内容"));
    }

    #[test]
    fn tool_result_to_json_error_format() {
        let result = ToolResult::error("测试错误");
        let json = result.to_json();
        assert!(json.contains("\"error\""));
        assert!(json.contains("测试错误"));
    }
}
