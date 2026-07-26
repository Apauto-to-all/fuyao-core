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
    /// 被压缩过的次数（每次 mark_compaction +1，用于精度降级提示）
    pub compression_count: i32,
    /// 最近一次压缩边界消息的 seq（NULL = 从未压缩）
    pub last_compacted_seq: Option<i64>,
    /// 通用子任务标记（非 fork 专属）：
    /// - `None` = 主 session（用户对话，`create_session` / `resume_session` 产出）
    /// - `Some(父 session id)` = 子任务 session（后台任务 / 子代理），值表示这个子任务隶属于哪个主 session
    ///
    /// 用途：前端区分主对话 vs 子任务，按父 id 分组/过滤。无论子任务是「全新创建」还是
    /// 「fork 旧的」来的，只要它是子任务就带 `parent_session_id`。
    ///
    /// 与历史「链式分裂压缩方案」的同名字段无任何关系——该方案已废弃，此处仅作通用子任务标记。
    pub parent_session_id: Option<String>,
    // 注：消息列表（messages）已从内存移除——每条消息产生即落 DB，
    // 需要时按 session_id 从数据库查询（见 SessionStore::load_visible_messages）。
    // 这样单个 session 内存占用恒定（不随历史增长），多 session 并发无内存压力。
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
            compression_count: 0,
            last_compacted_seq: None,
            parent_session_id: None,
        }
    }
}

/// 消息类型（区分普通消息与压缩边界消息）
///
/// `kind='compaction'` 的消息是上下文压缩产生的边界点，其 `content` 字段
/// 存摘要正文，模型可见窗口以此为下界过滤。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageKind {
    /// 普通消息（user / assistant / system / tool）
    #[default]
    Message,
    /// 压缩边界消息（content = 摘要正文）
    Compaction,
}

impl MessageKind {
    /// 序列化为数据库存储的字符串
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Compaction => "compaction",
        }
    }

    /// 从数据库字符串反序列化
    pub fn parse(s: &str) -> Self {
        match s {
            "compaction" => Self::Compaction,
            _ => Self::Message,
        }
    }
}

/// 消息角色（对话语义类型）
///
/// 四种变体覆盖所有主流 LLM 协议（OpenAI / Anthropic / Gemini / Bedrock）的角色语义：
/// - `User` / `Assistant` 是对话骨架
/// - `System` 表达操作者高权限指令
/// - `Tool` 表达工具执行结果
///
/// 序列化为小写字符串，与 DB 现有 TEXT 列数据、wire format 完全兼容——零迁移。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    /// 用户消息
    #[default]
    User,
    /// 助手消息
    Assistant,
    /// 工具结果消息
    Tool,
    /// 系统消息（含压缩边界消息，后者靠 `kind=Compaction` 区分）
    System,
}

impl MessageRole {
    /// 序列化为数据库存储 / wire format 的小写字符串
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
            Self::System => "system",
        }
    }

    /// 从数据库字符串反序列化，未知值兜底为 `User`
    ///
    /// DB 里可能存在历史脏数据（手动改库、旧版本残留、首字母大写的 `"Assistant"` 等），
    /// 反序列化遇到未知字符串时兜底为 `User`（不阻断流程）。可观测日志由调用方在
    /// 持有 DB 上下文的层（如 `fuyao_session::store::row`）记录——本函数保持纯函数，
    /// 与 `MessageKind::parse` 一致，避免给地基 crate 引入日志依赖。
    pub fn parse(s: &str) -> Self {
        match s {
            "user" => Self::User,
            "assistant" => Self::Assistant,
            "tool" => Self::Tool,
            "system" => Self::System,
            _ => Self::User,
        }
    }

    /// 判断是否为已知角色（非兜底值）
    ///
    /// 供调用方在 `parse` 后判断是否命中兜底，以便记录可观测日志。
    pub fn is_known(s: &str) -> bool {
        matches!(s, "user" | "assistant" | "tool" | "system")
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
    /// 角色：user / assistant / system / tool
    pub role: MessageRole,
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
    /// 会话内单调递增的投影序号（由 store 层填充，业务层只读）
    pub seq: i64,
    /// 消息类型（普通消息 / 压缩边界）
    pub kind: MessageKind,
}

impl Message {
    /// 创建用户消息
    pub fn user(content: String) -> Self {
        Self {
            role: MessageRole::User,
            content: Some(content),
            timestamp: current_timestamp(),
            ..Self::default()
        }
    }

    /// 创建 assistant 消息
    pub fn assistant(content: Option<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content,
            timestamp: current_timestamp(),
            ..Self::default()
        }
    }

    /// 创建工具结果消息
    pub fn tool_result(tool_call_id: String, content: String) -> Self {
        Self {
            role: MessageRole::Tool,
            tool_call_id: Some(tool_call_id),
            content: Some(content),
            timestamp: current_timestamp(),
            ..Self::default()
        }
    }

    /// 创建系统消息
    pub fn system(content: String) -> Self {
        Self {
            role: MessageRole::System,
            content: Some(content),
            timestamp: current_timestamp(),
            ..Self::default()
        }
    }

    /// 创建压缩边界消息（上下文压缩专用）
    ///
    /// `content` = 摘要正文（Markdown）；`role=System` 避免与 user/assistant
    /// 流混淆，`kind=Compaction` 是真正的类型标记（DB 列 + 业务识别都靠它）。
    pub fn compaction(summary: String) -> Self {
        Self {
            role: MessageRole::System,
            content: Some(summary),
            timestamp: current_timestamp(),
            kind: MessageKind::Compaction,
            ..Self::default()
        }
    }

    /// 转换为 OpenAI API 格式的 JSON Value
    pub fn to_openai(&self) -> serde_json::Value {
        let mut msg = serde_json::Map::new();
        msg.insert("role".to_string(), serde_json::json!(self.role.as_str()));

        match self.role {
            MessageRole::Assistant => {
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
            MessageRole::Tool => {
                if let Some(tool_call_id) = &self.tool_call_id {
                    msg.insert("tool_call_id".to_string(), serde_json::json!(tool_call_id));
                }
                msg.insert(
                    "content".to_string(),
                    serde_json::json!(self.content.as_deref().unwrap_or("")),
                );
            }
            MessageRole::User | MessageRole::System => {
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
    fn session_new_parent_session_id_defaults_none() {
        // 用户会话（非派生）：parent_session_id 应为 None
        let session = Session::new(None, None);
        assert!(session.parent_session_id.is_none());
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
        assert_eq!(msg.role, MessageRole::User);
        assert_eq!(msg.content, Some("你好".to_string()));
    }

    #[test]
    fn message_assistant_creates_assistant_role() {
        let msg = Message::assistant(Some("回复内容".to_string()));
        assert_eq!(msg.role, MessageRole::Assistant);
        assert_eq!(msg.content, Some("回复内容".to_string()));
    }

    #[test]
    fn message_tool_result_creates_tool_role() {
        let msg = Message::tool_result("call_123".to_string(), "结果".to_string());
        assert_eq!(msg.role, MessageRole::Tool);
        assert_eq!(msg.tool_call_id, Some("call_123".to_string()));
    }

    #[test]
    fn message_system_creates_system_role() {
        let msg = Message::system("系统提示".to_string());
        assert_eq!(msg.role, MessageRole::System);
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
            role: MessageRole::Assistant,
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
            role: MessageRole::Assistant,
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
        assert_eq!(msg.role, MessageRole::User);
    }

    #[test]
    fn todo_item_default_status_is_pending() {
        let item = TodoItem::default();
        assert_eq!(item.status, "pending");
    }

    #[test]
    fn message_kind_roundtrip() {
        assert_eq!(MessageKind::Message.as_str(), "message");
        assert_eq!(MessageKind::Compaction.as_str(), "compaction");
        assert_eq!(MessageKind::parse("message"), MessageKind::Message);
        assert_eq!(MessageKind::parse("compaction"), MessageKind::Compaction);
        // 未知字符串兜底为 Message
        assert_eq!(MessageKind::parse("unknown"), MessageKind::Message);
    }

    #[test]
    fn message_default_kind_is_message() {
        let msg = Message::default();
        assert_eq!(msg.kind, MessageKind::Message);
        assert_eq!(msg.seq, 0);
    }

    #[test]
    fn message_compaction_marks_kind() {
        let msg = Message::compaction("## 目标\n- 测试".to_string());
        assert_eq!(msg.kind, MessageKind::Compaction);
        assert_eq!(msg.role, MessageRole::System);
        assert_eq!(msg.content.as_deref(), Some("## 目标\n- 测试"));
    }

    #[test]
    fn message_role_roundtrip() {
        // as_str 输出小写，与 DB 存量数据 / wire format 一致
        assert_eq!(MessageRole::User.as_str(), "user");
        assert_eq!(MessageRole::Assistant.as_str(), "assistant");
        assert_eq!(MessageRole::Tool.as_str(), "tool");
        assert_eq!(MessageRole::System.as_str(), "system");

        // parse 正向解析
        assert_eq!(MessageRole::parse("user"), MessageRole::User);
        assert_eq!(MessageRole::parse("assistant"), MessageRole::Assistant);
        assert_eq!(MessageRole::parse("tool"), MessageRole::Tool);
        assert_eq!(MessageRole::parse("system"), MessageRole::System);

        // 未知值（首字母大写 / 脏数据）兜底为 User
        assert_eq!(MessageRole::parse("Assistant"), MessageRole::User);
        assert_eq!(MessageRole::parse("developer"), MessageRole::User);
        assert_eq!(MessageRole::parse("unknown"), MessageRole::User);

        // is_known 区分已知 / 未知
        assert!(MessageRole::is_known("user"));
        assert!(MessageRole::is_known("assistant"));
        assert!(MessageRole::is_known("tool"));
        assert!(MessageRole::is_known("system"));
        assert!(!MessageRole::is_known("Assistant"));
        assert!(!MessageRole::is_known("developer"));
    }

    #[test]
    fn message_role_default_is_user() {
        assert_eq!(MessageRole::default(), MessageRole::User);
    }

    #[test]
    fn message_role_serializes_lowercase() {
        // wire format：序列化输出小写字符串
        let json = serde_json::to_string(&MessageRole::Assistant).unwrap();
        assert_eq!(json, "\"assistant\"");
        let json = serde_json::to_string(&MessageRole::Tool).unwrap();
        assert_eq!(json, "\"tool\"");
    }
}
