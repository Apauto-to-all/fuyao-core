//! LLM Provider 统一抽象
//!
//! 所有 LLM 供应商实现此 trait，返回统一的 StreamEvent 流。
//! Engine 层只消费 StreamEvent，不关心底层 chunk 格式。

use async_trait::async_trait;
use futures_util::Stream;
use fuyao_api::{MessageRole, ThinkingType, ToolCallData, get_config};
use serde::{Deserialize, Serialize};
use std::pin::Pin;

/// 流式事件（Provider 层输出，Engine 层消费）
///
/// 每个 Provider 实现负责将自身 chunk 格式解码为此类型。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// 文本内容增量
    TextDelta { content: String },
    /// 推理内容增量
    ReasoningDelta { content: String },
    /// 工具调用增量（按 index 增量拼接 id/name/arguments，各字段独立到达）
    ToolCallChunk {
        index: usize,
        /// 工具调用 ID（通常在首个 chunk 中到达，部分供应商可能在后续 chunk 到达）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// 工具名称（通常在首个 chunk 中到达，部分供应商可能在后续 chunk 到达）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// 工具参数增量
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args_delta: Option<String>,
    },
    /// 流结束
    Done {
        usage: StreamUsage,
        finish_reason: FinishReason,
    },
}

/// 流式使用统计
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StreamUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_reasoning_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cached_tokens: Option<u32>,
    /// 提示词缓存写入桶的 token 数：prompt cache 写入侧的用量留痕，
    /// 仅作数据落点，不参与费用计算
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_creation_tokens: Option<u32>,
}

/// 完成原因
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
}

/// 流式选项
#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    /// 工具定义列表（中立形态，wire 编码归协议实现层）
    pub tools: Option<Vec<fuyao_api::ToolDefinition>>,
    /// 思考开关（对应 thinking.type 字段）。None 时不发，走模型默认
    ///
    /// **与 reasoning_effort 正交独立**：两者各自为 Some 时各自发送，互不压制。
    /// 禁止因本字段为 Disabled 而压掉 reasoning_effort——配了就必发，由服务器各自解释。
    pub thinking_type: Option<ThinkingType>,
    /// 思考强度档位名（用户自定义字符串，透传给服务器）。None 时不发，走模型默认
    ///
    /// **与 thinking_type 正交独立**：见 thinking_type 的约束说明。
    pub reasoning_effort: Option<String>,
}

/// 对话请求
#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub system: Option<String>,
}

/// 对话消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// 图片内容列表（多模态输入，user 消息专用；wire 适配在协议实现层）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<fuyao_api::ImageContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// 工具调用列表（assistant 消息专用，中立 typed 形态；wire 适配在协议实现层）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallData>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

impl Default for ChatMessage {
    fn default() -> Self {
        Self {
            role: MessageRole::User,
            content: None,
            images: vec![],
            reasoning: None,
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
        }
    }
}

/// 缺结果的 tool_call 占位内容——固定标记，模型据此得知该调用未产出结果（可决定重试）
pub const UNANSWERED_TOOL_RESULT_MARKER: &str = "[工具执行被拦截或中断]";

/// 为缺结果的 tool_call 补占位 tool_result（wire 协议配对不变量）
///
/// OpenAI / Anthropic 协议要求 assistant 消息的每个 tool_call 都有配对的 tool
/// 结果消息。被拦截 Block、中断、进程崩溃的工具调用不会落结果——消息序列进
/// wire 前统一过此函数：以全序列已存在的 tool 结果 id 集合为基准，为缺失项
/// 紧随其 assistant 消息之后插入一条占位 tool_result（content 为固定中断标记）。
/// 幂等：全部配对的序列原样通过，不增不改。
///
/// id 为空的 tool_call 跳过（无法构造配对关系）。
pub fn pair_missing_tool_results(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    // 全序列已有结果的 tool_call_id 集合（结果可能出现在序列任意位置；owned，
    // 不借用 messages——消息本体随后被循环消费移入结果序列）
    let answered_ids: std::collections::HashSet<String> = messages
        .iter()
        .filter(|m| matches!(m.role, MessageRole::Tool))
        .filter_map(|m| m.tool_call_id.clone())
        .collect();

    let mut paired = Vec::with_capacity(messages.len());
    for m in messages {
        // assistant 且带调用时，缺结果项先取所有权收走（借用在本体移入 paired 前结束；
        // 仅悬挂项付一次克隆成本，正常配对序列零开销）
        let missing: Vec<(String, String)> = if matches!(m.role, MessageRole::Assistant)
            && let Some(tool_calls) = m.tool_calls.as_ref()
        {
            tool_calls
                .iter()
                .filter(|tc| !tc.id.is_empty() && !answered_ids.contains(tc.id.as_str()))
                .map(|tc| (tc.id.clone(), tc.name.clone()))
                .collect()
        } else {
            Vec::new()
        };
        paired.push(m);
        for (id, name) in missing {
            paired.push(ChatMessage {
                role: MessageRole::Tool,
                content: Some(UNANSWERED_TOOL_RESULT_MARKER.to_string()),
                tool_call_id: Some(id),
                tool_name: Some(name),
                ..ChatMessage::default()
            });
        }
    }
    paired
}

/// 非流式对话响应
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: Option<String>,
    pub reasoning: Option<String>,
    pub tool_calls: Option<Vec<ToolCallData>>,
    pub usage: StreamUsage,
    pub finish_reason: FinishReason,
}

/// 流类型别名
pub type BoxStream<T> = Pin<Box<dyn Stream<Item = T> + Send>>;

/// LLM Provider 统一抽象
///
/// 所有 LLM 供应商实现此 trait。
/// stream_chat 返回 `BoxStream<StreamEvent>`，Engine 层统一消费。
#[async_trait]
pub trait Provider: Send + Sync {
    /// 流式对话，返回增量事件流
    fn stream_chat(
        &self,
        request: ChatRequest,
        model: &str,
        options: StreamOptions,
    ) -> BoxStream<Result<StreamEvent, StreamError>>;

    /// 非流式对话（用于标题生成等一次性短文本场景）
    ///
    /// 与 stream_chat 共享同一套 options（含思考开关 / 思考强度）——
    /// 非流式仅指响应一次性返回，思考能力与流式路径等价支持。
    async fn chat(
        &self,
        request: ChatRequest,
        model: &str,
        options: StreamOptions,
    ) -> Result<ChatResponse, StreamError>;
}

/// 流式错误
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("API 请求失败: {message}")]
    ApiError {
        /// HTTP 状态码。供程序精确判断可重试性。
        /// 协议层异常（HTTP 200 但响应体异常）无对应状态码，为 None。
        status: Option<u16>,
        /// 人类可读错误描述（含 "HTTP {code}: " 前缀时与 status 对应），供日志/错误事件展示。
        message: String,
    },
    #[error("流式响应解析失败: {0}")]
    StreamParseError(String),
    #[error("连接超时")]
    Timeout,
    #[error("认证失败: {0}")]
    AuthError(String),
    #[error("速率限制")]
    RateLimit {
        /// HTTP `retry-after-ms` 响应头（毫秒）
        retry_after_ms: Option<u64>,
        /// HTTP `retry-after` 响应头（秒）
        retry_after_secs: Option<u64>,
    },
    #[error("连接错误: {0}")]
    Connection(String),
    #[error("上下文溢出")]
    ContextOverflow,
    /// 操作被取消（如引擎关闭）。非错误，不应触发重试
    ///
    /// 由 `RetryRunner` 在退避 sleep 期间收到 shutdown 信号时产生，
    /// 冒泡到 `turn.rs` 后由 shutdown 分支走中断路径（不发 Error 事件）。
    #[error("操作被取消")]
    Cancelled,
}

/// 退避时长参数（从全局配置 `get_config().llm.retry` 读取）
///
/// 字段含义：
/// - `initial_delay_ms`：首次重试前的等待毫秒数（默认 2000）
/// - `max_delay_ms`：无 Retry-After 响应头场景下的退避上限（默认 30000）
/// - `max_delay_with_headers_ms`：有 Retry-After 响应头场景下的退避上限（默认 i64::MAX）
struct BackoffParams {
    initial_delay_ms: u64,
    max_delay_ms: u64,
    max_delay_with_headers_ms: u64,
}

impl BackoffParams {
    fn from_config() -> Self {
        let r = &get_config().llm.retry;
        Self {
            initial_delay_ms: r.initial_delay_ms,
            max_delay_ms: r.max_delay_ms,
            max_delay_with_headers_ms: r.max_delay_with_headers_ms,
        }
    }
}

impl StreamError {
    /// 是否可重试
    ///
    /// 可重试：RateLimit、Timeout、Connection、5xx ApiError（500/502/503/504/529）。
    /// 不可重试：AuthError、StreamParseError、ContextOverflow、4xx ApiError、Cancelled、
    ///          无状态码的 ApiError（协议层异常，非 HTTP 错误）。
    ///
    /// 重试策略与错误类型同处——`StreamError` 的字段形状本就为退避决策而设
    ///（`RateLimit.retry_after_*`、`ApiError.status`），把策略挂回错误类型让数据与
    /// 分支在同模块可一起改，消除原先错误字段在 provider.rs、策略在 retry.rs 的跨模块耦合。
    pub fn is_retryable(&self) -> bool {
        match self {
            StreamError::RateLimit { .. } | StreamError::Timeout | StreamError::Connection(_) => {
                true
            }
            StreamError::ApiError {
                status: Some(code), ..
            } => matches!(code, 500 | 502 | 503 | 504 | 529),
            StreamError::ApiError { status: None, .. } => false,
            // ContextOverflow 不重试，应触发压缩
            // Cancelled 不重试（非错误，由 shutdown 流程触发，冒泡给上层走中断路径）
            StreamError::ContextOverflow
            | StreamError::AuthError(_)
            | StreamError::StreamParseError(_)
            | StreamError::Cancelled => false,
        }
    }

    /// 计算退避时长（双分支策略）
    ///
    /// 优先级：
    /// 1. retry-after-ms 响应头
    /// 2. retry-after 响应头
    /// 3. 有响应头（RateLimit/5xx）→ 指数退避，上限 ~24.8天
    /// 4. 无响应头（Timeout/Connection）→ 指数退避，上限 30s
    pub fn backoff_duration(&self, retry: u32) -> std::time::Duration {
        let p = BackoffParams::from_config();

        // 优先级 1: retry-after-ms 响应头
        if let StreamError::RateLimit {
            retry_after_ms: Some(ms),
            ..
        } = self
        {
            return std::time::Duration::from_millis((*ms).min(p.max_delay_with_headers_ms));
        }

        // 优先级 2: retry-after 响应头（秒 → 毫秒）
        if let StreamError::RateLimit {
            retry_after_secs: Some(secs),
            ..
        } = self
        {
            return std::time::Duration::from_millis(
                (secs * 1000).min(p.max_delay_with_headers_ms),
            );
        }

        // 优先级 3 & 4: 指数退避
        let base = p
            .initial_delay_ms
            .saturating_mul(2u64.saturating_pow(retry - 1));

        // 有响应头的错误（RateLimit、带 HTTP 状态码的 ApiError）→ 上限 ~24.8天
        let has_headers = matches!(
            self,
            StreamError::RateLimit { .. }
                | StreamError::ApiError {
                    status: Some(_),
                    ..
                }
        );

        if has_headers {
            std::time::Duration::from_millis(base.min(p.max_delay_with_headers_ms))
        } else {
            // 无响应头的错误（Timeout、Connection）→ 上限 30s
            std::time::Duration::from_millis(base.min(p.max_delay_ms))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_event_text_delta_serializes() {
        let event = StreamEvent::TextDelta {
            content: "Hello".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"text_delta\""));
    }

    #[test]
    fn stream_event_tool_call_chunk_serializes() {
        let event = StreamEvent::ToolCallChunk {
            index: 0,
            id: Some("call_1".to_string()),
            name: Some("read_file".to_string()),
            args_delta: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"tool_call_chunk\""));
        assert!(json.contains("\"id\":\"call_1\""));
        assert!(json.contains("\"name\":\"read_file\""));
        assert!(!json.contains("args_delta"));
    }

    #[test]
    fn stream_usage_default_is_zero() {
        let usage = StreamUsage::default();
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.completion_tokens, 0);
    }

    #[test]
    fn stream_usage_cache_creation_none_not_serialized() {
        let usage = StreamUsage::default();
        let json = serde_json::to_string(&usage).unwrap();
        assert!(!json.contains("prompt_cache_creation_tokens"));
    }

    #[test]
    fn stream_usage_cache_creation_some_serialized() {
        let usage = StreamUsage {
            prompt_cache_creation_tokens: Some(42),
            ..Default::default()
        };
        let json = serde_json::to_string(&usage).unwrap();
        assert!(json.contains("\"prompt_cache_creation_tokens\":42"));
    }

    #[test]
    fn chat_message_default_role_is_user() {
        let msg = ChatMessage::default();
        assert_eq!(msg.role, MessageRole::User);
    }

    #[test]
    fn finish_reason_serializes_snake_case() {
        let reason = FinishReason::ToolCalls;
        let json = serde_json::to_string(&reason).unwrap();
        assert!(json.contains("\"tool_calls\""));
    }

    // ── StreamError 重试策略（is_retryable / backoff_duration）──────────────

    #[test]
    fn is_retryable_rate_limit() {
        assert!(
            StreamError::RateLimit {
                retry_after_ms: None,
                retry_after_secs: None
            }
            .is_retryable()
        );
    }

    #[test]
    fn is_retryable_timeout() {
        assert!(StreamError::Timeout.is_retryable());
    }

    #[test]
    fn is_retryable_connection() {
        assert!(StreamError::Connection("refused".to_string()).is_retryable());
    }

    #[test]
    fn is_retryable_5xx_api_error() {
        for code in [500, 502, 503, 504, 529] {
            assert!(
                StreamError::ApiError {
                    status: Some(code),
                    message: format!("HTTP {code}")
                }
                .is_retryable()
            );
        }
    }

    #[test]
    fn is_not_retryable_4xx_api_error() {
        for code in [400, 401, 404] {
            assert!(
                !StreamError::ApiError {
                    status: Some(code),
                    message: format!("HTTP {code}")
                }
                .is_retryable()
            );
        }
    }

    #[test]
    fn is_not_retryable_api_error_without_status() {
        // 协议层异常（无 HTTP 状态码）不可重试
        assert!(
            !StreamError::ApiError {
                status: None,
                message: "响应中无 choice".to_string()
            }
            .is_retryable()
        );
    }

    #[test]
    fn is_not_retryable_auth_error() {
        assert!(!StreamError::AuthError("invalid key".to_string()).is_retryable());
    }

    #[test]
    fn is_not_retryable_stream_parse_error() {
        assert!(!StreamError::StreamParseError("invalid json".to_string()).is_retryable());
    }

    #[test]
    fn is_not_retryable_context_overflow() {
        assert!(!StreamError::ContextOverflow.is_retryable());
    }

    #[test]
    fn is_not_retryable_cancelled() {
        // 取消不是错误（shutdown 触发），不应重试，应立即冒泡给上层走中断路径
        assert!(!StreamError::Cancelled.is_retryable());
    }

    #[test]
    fn backoff_duration_rate_limit_with_retry_after_ms() {
        // 优先级1: retry-after-ms 响应头
        let error = StreamError::RateLimit {
            retry_after_ms: Some(5000),
            retry_after_secs: None,
        };
        assert_eq!(
            error.backoff_duration(1),
            std::time::Duration::from_millis(5000)
        );
    }

    #[test]
    fn backoff_duration_rate_limit_with_retry_after_secs() {
        // 优先级2: retry-after 响应头（秒 → 毫秒）
        let error = StreamError::RateLimit {
            retry_after_ms: None,
            retry_after_secs: Some(10),
        };
        assert_eq!(
            error.backoff_duration(1),
            std::time::Duration::from_millis(10000)
        );
    }

    #[test]
    fn backoff_duration_rate_limit_exponential() {
        // 优先级3: 有响应头 → 指数退避，上限 ~24.8天
        let error = StreamError::RateLimit {
            retry_after_ms: None,
            retry_after_secs: None,
        };
        assert_eq!(
            error.backoff_duration(1),
            std::time::Duration::from_millis(2000)
        );
        assert_eq!(
            error.backoff_duration(5),
            std::time::Duration::from_millis(32000)
        );
        // 第20次: 2000 * 2^19 = 1048576000ms ≈ 12天，未超上限
        assert_eq!(
            error.backoff_duration(20),
            std::time::Duration::from_millis(1048576000)
        );
        // 第31次: 2000 * 2^30 = 2147483648000ms，超过上限 → 封顶
        assert_eq!(
            error.backoff_duration(31),
            std::time::Duration::from_millis(2_147_483_647)
        );
    }

    #[test]
    fn backoff_duration_timeout_capped_at_30s() {
        // 优先级4: 无响应头 → 上限 30s
        let error = StreamError::Timeout;
        assert_eq!(
            error.backoff_duration(1),
            std::time::Duration::from_millis(2000)
        );
        assert_eq!(
            error.backoff_duration(5),
            std::time::Duration::from_millis(30000) // 32s 被 30s 上限截断
        );
    }

    #[test]
    fn backoff_duration_connection_capped_at_30s() {
        let error = StreamError::Connection("refused".to_string());
        assert_eq!(
            error.backoff_duration(5),
            std::time::Duration::from_millis(30000)
        );
    }

    #[test]
    fn backoff_duration_5xx_api_error_has_headers() {
        // 5xx ApiError（带 HTTP 状态码）有响应头 → 上限 ~24.8天
        let error = StreamError::ApiError {
            status: Some(503),
            message: "HTTP 503: Service Unavailable".to_string(),
        };
        assert_eq!(
            error.backoff_duration(5),
            std::time::Duration::from_millis(32000)
        );
        // 第20次: 未超上限
        assert_eq!(
            error.backoff_duration(20),
            std::time::Duration::from_millis(1048576000)
        );
        // 第31次: 超过上限 → 封顶
        assert_eq!(
            error.backoff_duration(31),
            std::time::Duration::from_millis(2_147_483_647)
        );
    }

    // ===== pair_missing_tool_results 测试 =====

    /// 构造带 tool_calls 的 assistant 消息
    fn assistant_with_calls(ids: &[&str]) -> ChatMessage {
        ChatMessage {
            role: MessageRole::Assistant,
            content: None,
            tool_calls: Some(
                ids.iter()
                    .map(|id| ToolCallData {
                        id: id.to_string(),
                        name: format!("tool_{id}"),
                        arguments: "{}".to_string(),
                    })
                    .collect(),
            ),
            ..ChatMessage::default()
        }
    }

    /// 构造 tool 结果消息
    fn tool_result(id: &str) -> ChatMessage {
        ChatMessage {
            role: MessageRole::Tool,
            content: Some("结果".to_string()),
            tool_call_id: Some(id.to_string()),
            tool_name: Some(format!("tool_{id}")),
            ..ChatMessage::default()
        }
    }

    #[test]
    fn pairing_supplements_missing_after_assistant() {
        // assistant 带 3 个调用，仅 c2 有结果 → c1/c3 紧随 assistant 之后补占位
        let messages = vec![
            ChatMessage {
                role: MessageRole::User,
                content: Some("问题".to_string()),
                ..ChatMessage::default()
            },
            assistant_with_calls(&["c1", "c2", "c3"]),
            tool_result("c2"),
        ];

        let paired = pair_missing_tool_results(messages);
        // user + assistant + 占位c1 + 占位c3 + 结果c2 = 5
        //（占位紧随 assistant 批量插入，既有结果保持原相对位置排在其后）
        assert_eq!(paired.len(), 5);
        // 占位 c1 位于 assistant 之后（索引 2）
        assert_eq!(paired[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(
            paired[2].content.as_deref(),
            Some(UNANSWERED_TOOL_RESULT_MARKER)
        );
        assert_eq!(paired[2].tool_name.as_deref(), Some("tool_c1"));
        assert!(matches!(paired[2].role, MessageRole::Tool));
        // 占位 c3 紧随其后（索引 3），content 同为固定标记
        assert_eq!(paired[3].tool_call_id.as_deref(), Some("c3"));
        assert_eq!(
            paired[3].content.as_deref(),
            Some(UNANSWERED_TOOL_RESULT_MARKER)
        );
        // 原有结果原样保留（末位），内容不被改写
        assert_eq!(paired[4].tool_call_id.as_deref(), Some("c2"));
        assert_eq!(paired[4].content.as_deref(), Some("结果"));
    }

    #[test]
    fn pairing_no_op_when_all_answered() {
        // 全部调用有结果 → 序列原样通过，不增不改
        let messages = vec![
            assistant_with_calls(&["c1", "c2"]),
            tool_result("c1"),
            tool_result("c2"),
        ];
        let paired = pair_missing_tool_results(messages);
        assert_eq!(paired.len(), 3);
        assert!(
            paired
                .iter()
                .all(|m| m.content.as_deref() != Some(UNANSWERED_TOOL_RESULT_MARKER)),
            "不应插入占位"
        );
    }

    #[test]
    fn pairing_ignores_assistant_without_tool_calls() {
        // 无调用的 assistant / 纯 user 对话不触发补充
        let messages = vec![
            ChatMessage {
                role: MessageRole::User,
                content: Some("你好".to_string()),
                ..ChatMessage::default()
            },
            ChatMessage {
                role: MessageRole::Assistant,
                content: Some("回复".to_string()),
                ..ChatMessage::default()
            },
        ];
        let paired = pair_missing_tool_results(messages);
        assert_eq!(paired.len(), 2);
    }

    #[test]
    fn pairing_skips_empty_id() {
        // id 为空的调用无法构造配对关系，跳过不补
        let mut assistant = assistant_with_calls(&[""]);
        assistant.tool_calls = Some(vec![ToolCallData {
            id: String::new(),
            name: "bad".to_string(),
            arguments: "{}".to_string(),
        }]);
        let paired = pair_missing_tool_results(vec![assistant]);
        assert_eq!(paired.len(), 1, "空 id 不补占位");
    }

    #[test]
    fn pairing_handles_multiple_assistant_messages() {
        // 多个 assistant 批次各自独立配对：第一批全有结果，第二批全缺
        let messages = vec![
            assistant_with_calls(&["a1"]),
            tool_result("a1"),
            assistant_with_calls(&["b1", "b2"]),
        ];
        let paired = pair_missing_tool_results(messages);
        // 第一批(2) + 第二批 assistant(1) + 两个占位(2) = 5
        assert_eq!(paired.len(), 5);
        let supplemented: Vec<&str> = paired
            .iter()
            .filter(|m| m.content.as_deref() == Some(UNANSWERED_TOOL_RESULT_MARKER))
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        assert_eq!(supplemented, vec!["b1", "b2"]);
    }
}
