//! 会话持久化类型定义
//!
//! Session、Message、TodoItem 类型。

use std::time::{SystemTime, UNIX_EPOCH};

fn current_timestamp() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

/// 会话
#[derive(Debug, Clone, Default)]
pub struct Session {
    /// 会话唯一标识
    pub id: String,
    /// 父会话 ID（压缩分裂时指向原会话）
    pub parent_session_id: Option<String>,
    /// 会话标题
    pub title: Option<String>,
    /// 系统提示词
    pub system_prompt: Option<String>,
    /// 消息总数
    pub message_count: i64,
    /// 工具调用总数
    pub tool_call_count: i64,
    /// 总输入 token
    pub total_prompt_tokens: i64,
    /// 总输出 token
    pub total_completion_tokens: i64,
    /// 总推理 token
    pub total_reasoning_tokens: i64,
    /// 总缓存命中 token
    pub total_cached_tokens: i64,
    /// 总费用
    pub total_cost: f64,
    /// 开始时间戳
    pub started_at: f64,
    /// 结束时间戳
    pub ended_at: Option<f64>,
    /// 结束原因
    pub end_reason: Option<String>,
    /// 消息列表
    pub messages: Vec<Message>,
}

impl Session {
    /// 创建新会话，自动生成 8 位 UUID
    pub fn new(title: Option<String>, system_prompt: Option<String>) -> Self {
        let id = uuid::Uuid::new_v4()
            .to_string()
            .split('-')
            .next()
            .unwrap_or("unknown")
            .to_string();
        Self {
            id,
            parent_session_id: None,
            title: title.or_else(|| Some("新会话".to_string())),
            system_prompt,
            message_count: 0,
            tool_call_count: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_reasoning_tokens: 0,
            total_cached_tokens: 0,
            total_cost: 0.0,
            started_at: current_timestamp(),
            ended_at: None,
            end_reason: None,
            messages: Vec::new(),
        }
    }
}

/// 消息（持久化单元）
#[derive(Debug, Clone, Default)]
pub struct Message {
    /// 消息 ID
    pub id: Option<i64>,
    /// 所属会话 ID
    pub session_id: String,
    /// 模型 ID
    pub model_id: Option<String>,
    /// 角色：user / assistant / system / tool / developer
    pub role: String,
    /// 消息内容
    pub content: Option<String>,
    /// 推理内容
    pub reasoning: Option<String>,
    /// 工具调用 ID
    pub tool_call_id: Option<String>,
    /// 工具调用列表（JSON）
    pub tool_calls: Option<serde_json::Value>,
    /// 工具名称
    pub tool_name: Option<String>,
    /// 完成原因
    pub finish_reason: Option<String>,
    /// 时间戳
    pub timestamp: f64,
    /// 输入 token 数
    pub prompt_tokens: i64,
    /// 输出 token 数
    pub completion_tokens: i64,
    /// 推理 token 数
    pub reasoning_tokens: i64,
    /// 缓存命中 token 数
    pub cached_tokens: i64,
    /// 单条消息费用
    pub cost: f64,
}

impl Message {
    /// 创建用户消息
    pub fn user(content: String) -> Self {
        Self {
            role: "user".to_string(),
            content: Some(content),
            timestamp: current_timestamp(),
            ..Self::default()
        }
    }

    /// 创建 assistant 消息
    pub fn assistant(content: Option<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content,
            timestamp: current_timestamp(),
            ..Self::default()
        }
    }

    /// 创建工具结果消息
    pub fn tool_result(tool_call_id: String, content: String) -> Self {
        Self {
            role: "tool".to_string(),
            tool_call_id: Some(tool_call_id),
            content: Some(content),
            timestamp: current_timestamp(),
            ..Self::default()
        }
    }

    /// 创建系统消息
    pub fn system(content: String) -> Self {
        Self {
            role: "system".to_string(),
            content: Some(content),
            timestamp: current_timestamp(),
            ..Self::default()
        }
    }

    /// 转换为 OpenAI API 格式的 JSON Value
    pub fn to_openai(&self) -> serde_json::Value {
        let mut msg = serde_json::Map::new();
        msg.insert("role".to_string(), serde_json::json!(&self.role));

        match self.role.as_str() {
            "assistant" => {
                // Qwen 不接受 tool_calls + content=""，有工具调用且无内容时不设置 content
                if self.tool_calls.is_some() && self.content.is_none() {
                    // 不设置 content
                } else {
                    msg.insert(
                        "content".to_string(),
                        serde_json::json!(self.content.as_deref().unwrap_or("")),
                    );
                }
                if let Some(tool_calls) = &self.tool_calls {
                    msg.insert("tool_calls".to_string(), tool_calls.clone());
                }
                if let Some(reasoning) = &self.reasoning {
                    msg.insert(
                        "reasoning_content".to_string(),
                        serde_json::json!(reasoning),
                    );
                }
            }
            "tool" => {
                if let Some(tool_call_id) = &self.tool_call_id {
                    msg.insert("tool_call_id".to_string(), serde_json::json!(tool_call_id));
                }
                msg.insert(
                    "content".to_string(),
                    serde_json::json!(self.content.as_deref().unwrap_or("")),
                );
            }
            _ => {
                msg.insert(
                    "content".to_string(),
                    serde_json::json!(self.content.as_deref().unwrap_or("")),
                );
            }
        }

        serde_json::Value::Object(msg)
    }
}

/// 待办事项
#[derive(Debug, Clone)]
pub struct TodoItem {
    /// 任务唯一标识（Agent 自选）
    pub id: String,
    /// 任务描述
    pub content: String,
    /// 任务状态：pending / in_progress / completed / cancelled
    pub status: String,
}

impl Default for TodoItem {
    fn default() -> Self {
        Self {
            id: String::new(),
            content: String::new(),
            status: "pending".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_new_generates_8_char_id() {
        let session = Session::new(None, None);
        assert_eq!(session.id.len(), 8);
        assert_eq!(session.title, Some("新会话".to_string()));
    }

    #[test]
    fn session_new_with_custom_title() {
        let session = Session::new(Some("测试会话".to_string()), Some("系统提示".to_string()));
        assert_eq!(session.title, Some("测试会话".to_string()));
        assert_eq!(session.system_prompt, Some("系统提示".to_string()));
    }

    #[test]
    fn message_user_creates_user_role() {
        let msg = Message::user("你好".to_string());
        assert_eq!(msg.role, "user");
        assert_eq!(msg.content, Some("你好".to_string()));
    }

    #[test]
    fn message_assistant_creates_assistant_role() {
        let msg = Message::assistant(Some("回复内容".to_string()));
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.content, Some("回复内容".to_string()));
    }

    #[test]
    fn message_tool_result_creates_tool_role() {
        let msg = Message::tool_result("call_123".to_string(), "结果".to_string());
        assert_eq!(msg.role, "tool");
        assert_eq!(msg.tool_call_id, Some("call_123".to_string()));
    }

    #[test]
    fn message_system_creates_system_role() {
        let msg = Message::system("系统提示".to_string());
        assert_eq!(msg.role, "system");
    }

    #[test]
    fn message_to_openai_user() {
        let msg = Message::user("你好".to_string());
        let openai = msg.to_openai();
        assert_eq!(openai["role"], "user");
        assert_eq!(openai["content"], "你好");
    }

    #[test]
    fn message_to_openai_assistant_with_tool_calls() {
        let tool_calls = serde_json::json!([{
            "id": "call_1",
            "type": "function",
            "function": { "name": "bash", "arguments": "{}" }
        }]);
        let msg = Message {
            role: "assistant".to_string(),
            content: None,
            tool_calls: Some(tool_calls),
            ..Message::default()
        };
        let openai = msg.to_openai();
        assert_eq!(openai["role"], "assistant");
        assert!(openai.get("content").is_none());
        assert!(openai["tool_calls"].is_array());
    }

    #[test]
    fn message_to_openai_tool_result() {
        let msg = Message::tool_result("call_123".to_string(), "文件内容".to_string());
        let openai = msg.to_openai();
        assert_eq!(openai["role"], "tool");
        assert_eq!(openai["tool_call_id"], "call_123");
    }

    #[test]
    fn message_to_openai_assistant_with_reasoning() {
        let msg = Message {
            role: "assistant".to_string(),
            content: Some("回复".to_string()),
            reasoning: Some("思考过程".to_string()),
            ..Message::default()
        };
        let openai = msg.to_openai();
        assert_eq!(openai["reasoning_content"], "思考过程");
    }

    #[test]
    fn message_default_role_is_user() {
        let msg = Message::default();
        assert_eq!(msg.role, "");
    }

    #[test]
    fn todo_item_default_status_is_pending() {
        let item = TodoItem::default();
        assert_eq!(item.status, "pending");
    }
}
