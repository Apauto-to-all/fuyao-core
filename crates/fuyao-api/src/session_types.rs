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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Session {
    /// 会话唯一标识
    pub id: String,
    /// 会话标题
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 系统提示词
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<f64>,
    /// 结束原因
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
    /// 被压缩过的次数（每次 mark_compaction +1，用于精度降级提示）
    pub compression_count: i32,
    /// 最近一次压缩边界消息的 seq（NULL = 从未压缩）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compacted_seq: Option<i64>,
    /// 通用子任务标记（非 fork 专属）：
    /// - `None` = 主 session（用户对话，`create_session` / `resume_session` 产出）
    /// - `Some(父 session id)` = 子任务 session（后台任务 / 子代理），值表示这个子任务隶属于哪个主 session
    ///
    /// 用途：前端区分主对话 vs 子任务，按父 id 分组/过滤。无论子任务是「全新创建」还是
    /// 「fork 旧的」来的，只要它是子任务就带 `parent_session_id`。
    ///
    /// 与历史「链式分裂压缩方案」的同名字段无任何关系——该方案已废弃，此处仅作通用子任务标记。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// 工作目录绝对路径（创建会话时定死，不再变化）
    ///
    /// 来源：引擎创建会话时的 `agent_paths.workspace`。无工作目录（纯 global 层运行）时为 `None`。
    /// 用途：session 列表查询按项目过滤——同一 agent（同一 db）下不同工作目录的会话靠此字段区分。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// 最近活动时间（Unix 秒浮点）
    ///
    /// 创建时等于 `started_at`；每次会话交互后（`SessionStore::update` 落库时）刷新为当前时间。
    /// 用途：session 列表查询按最近活动倒序——用户刚交互的会话排最前（类即时通讯的「最近会话」）。
    pub last_active_at: f64,
    // 注：消息列表（messages）已从内存移除——每条消息产生即落 DB，
    // 需要时按 session_id 从数据库查询（见 SessionStore::load_visible_messages）。
    // 这样单个 session 内存占用恒定（不随历史增长），多 session 并发无内存压力。
}

impl Session {
    /// 创建新会话，自动生成 8 位 id
    ///
    /// `workspace` 为工作目录绝对路径（创建时定死，来自引擎的 `agent_paths.workspace`），
    /// 无工作目录时传 `None`。`last_active_at` 初始化为当前时间（等于 `started_at`）。
    pub fn new(
        workspace: Option<String>,
        title: Option<String>,
        system_prompt: Option<String>,
    ) -> Self {
        let now = current_timestamp();
        Self {
            id: generate_id(),
            title: title.or_else(|| Some("新会话".to_string())),
            system_prompt,
            message_count: 0,
            tool_call_count: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_reasoning_tokens: 0,
            total_cached_tokens: 0,
            total_cost: 0.0,
            started_at: now,
            last_active_at: now,
            ended_at: None,
            end_reason: None,
            compression_count: 0,
            last_compacted_seq: None,
            parent_session_id: None,
            workspace,
        }
    }

    /// 重新生成 id（主键冲突重试专用）
    ///
    /// id 由随机生成，与既有行碰撞时（概率极低）由 store 层的 `create_with_retry`
    /// 调本方法换一个新 id 重试落库。不改动其它字段。
    pub fn regenerate_id(&mut self) {
        self.id = generate_id();
    }
}

/// 生成 8 位会话 id：取 UUID v4 第一段（8 个十六进制字符，32 bit 熵）
///
/// 个人单用户场景下碰撞概率可忽略；DB 主键约束兜底，碰撞时上层重试（见
/// `SessionStore::create_with_retry`）。
fn generate_id() -> String {
    uuid::Uuid::new_v4()
        .to_string()
        .split('-')
        .next()
        .unwrap_or("unknown")
        .to_string()
}

/// 消息类型（区分普通消息与压缩边界消息）
///
/// `kind='compaction'` 的消息是上下文压缩产生的边界点，其 `content` 字段
/// 存摘要正文，模型可见窗口以此为下界过滤。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// 系统消息（操作者高权限指令，对应 [`Message::system`] 构造）
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

/// 图片内容块（多模态输入）
///
/// `data` 存**裸 base64**（不含 `data:` 前缀），`mime_type` 为图片 MIME 类型。
/// 全链路（事件 → 落库 → 请求构造）统一此形态；data URL 在入站时解析归一化。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ImageContent {
    /// 图片 MIME 类型（如 `image/png` / `image/jpeg` / `image/webp`）
    pub mime_type: String,
    /// 图片数据（裸 base64，不含 `data:` 前缀）
    pub data: String,
}

impl ImageContent {
    /// 从 data URL 解析图片内容（`data:image/png;base64,xxxx`）
    ///
    /// 入站宽容：接受 data URL 或裸 base64 + 显式 mime（后者直接构造本类型）。
    /// mime 段允许带参数（如 `data:image/png;charset=utf-8;base64,xxx`），取第一段。
    /// 解析失败（非 data URL / mime 或数据为空）返回 `None`。
    pub fn from_data_url(s: &str) -> Option<Self> {
        let rest = s.strip_prefix("data:")?;
        let (mime, b64) = rest.split_once(',')?;
        if b64.is_empty() {
            return None;
        }
        let mime = mime.split(';').next().unwrap_or("").trim();
        if mime.is_empty() {
            return None;
        }
        Some(Self {
            mime_type: mime.to_string(),
            data: b64.to_string(),
        })
    }
}

/// 消息（持久化单元）
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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
    /// 图片内容列表（多模态输入，user 消息专用；其余角色恒为空）
    pub images: Vec<ImageContent>,
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

    /// 创建带图片的用户消息（多模态输入）
    ///
    /// 纯文本路径仍用 `user()`，图片是旁挂增量，两者并存。
    pub fn user_with_images(content: String, images: Vec<ImageContent>) -> Self {
        Self {
            role: MessageRole::User,
            content: Some(content),
            images,
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
        let session = Session::new(None, None, None);
        assert_eq!(session.id.len(), 8);
        assert_eq!(session.title, Some("新会话".to_string()));
    }

    #[test]
    fn regenerate_id_produces_new_8_char_id() {
        // 重试专用：regenerate_id 应换一个新 id，长度仍为 8，其它字段不变
        let mut session = Session::new(None, Some("标题".into()), None);
        let old_id = session.id.clone();
        session.regenerate_id();
        assert_eq!(session.id.len(), 8);
        assert_ne!(session.id, old_id, "regenerate_id 必须产生不同的 id");
        assert_eq!(session.title.as_deref(), Some("标题"), "其它字段不应被改动");
    }

    #[test]
    fn session_new_parent_session_id_defaults_none() {
        // 用户会话（非派生）：parent_session_id 应为 None
        let session = Session::new(None, None, None);
        assert!(session.parent_session_id.is_none());
    }

    #[test]
    fn session_new_with_custom_title() {
        let session = Session::new(
            None,
            Some("测试会话".to_string()),
            Some("系统提示".to_string()),
        );
        assert_eq!(session.title, Some("测试会话".to_string()));
        assert_eq!(session.system_prompt, Some("系统提示".to_string()));
    }

    #[test]
    fn session_new_workspace_and_last_active_at() {
        // workspace 创建时定死；last_active_at 初始化等于 started_at
        let session = Session::new(Some("/home/u/proj".to_string()), None, None);
        assert_eq!(session.workspace.as_deref(), Some("/home/u/proj"));
        assert_eq!(session.started_at, session.last_active_at);
    }

    #[test]
    fn session_new_workspace_none_when_absent() {
        let session = Session::new(None, None, None);
        assert!(session.workspace.is_none());
    }

    #[test]
    fn message_user_creates_user_role() {
        let msg = Message::user("你好".to_string());
        assert_eq!(msg.role, MessageRole::User);
        assert_eq!(msg.content, Some("你好".to_string()));
        assert!(msg.images.is_empty());
    }

    #[test]
    fn message_user_with_images_holds_images() {
        let img = ImageContent {
            mime_type: "image/png".into(),
            data: "aGVsbG8=".into(),
        };
        let msg = Message::user_with_images("看图".to_string(), vec![img.clone()]);
        assert_eq!(msg.role, MessageRole::User);
        assert_eq!(msg.content, Some("看图".to_string()));
        assert_eq!(msg.images, vec![img]);
    }

    #[test]
    fn message_user_with_images_empty_images() {
        let msg = Message::user_with_images("纯文本".to_string(), vec![]);
        assert_eq!(msg.content, Some("纯文本".to_string()));
        assert!(msg.images.is_empty());
    }

    #[test]
    fn image_content_default_message_images_empty() {
        // 无图消息的 images 恒为空
        let msg = Message::assistant(Some("回复".to_string()));
        assert!(msg.images.is_empty());
    }

    #[test]
    fn image_content_from_data_url_parses() {
        let img = ImageContent::from_data_url("data:image/png;base64,aGVsbG8=").unwrap();
        assert_eq!(img.mime_type, "image/png");
        assert_eq!(img.data, "aGVsbG8=");
    }

    #[test]
    fn image_content_from_data_url_with_mime_params() {
        // mime 段带参数时只取第一段
        let img =
            ImageContent::from_data_url("data:image/jpeg;charset=utf-8;base64,aGVsbG8=").unwrap();
        assert_eq!(img.mime_type, "image/jpeg");
        assert_eq!(img.data, "aGVsbG8=");
    }

    #[test]
    fn image_content_from_data_url_rejects_invalid() {
        // 非 data URL / 缺 mime / 空数据 均解析失败
        assert!(ImageContent::from_data_url("http://example.com/a.png").is_none());
        assert!(ImageContent::from_data_url("data:;base64,aGVsbG8=").is_none());
        assert!(ImageContent::from_data_url("data:image/png;base64,").is_none());
        assert!(ImageContent::from_data_url("data:image/png").is_none());
        assert!(ImageContent::from_data_url("").is_none());
    }

    #[test]
    fn image_content_serde_roundtrip() {
        let img = ImageContent {
            mime_type: "image/webp".into(),
            data: "d2VicA==".into(),
        };
        let json = serde_json::to_string(&img).unwrap();
        let de: ImageContent = serde_json::from_str(&json).unwrap();
        assert_eq!(de, img);
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
