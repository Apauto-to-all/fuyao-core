//! 执行层：调 provider 生成摘要 + 失败处理
//!
//! 关键约束（保留前缀缓存）：
//! - **消息原样发**：所有 user/assistant/tool 消息保持原 role/content/tool_calls 不变
//! - **system 不变**：用 session 原本的 system_prompt（前缀缓存完整命中）
//! - **末尾追加一条 user 消息**：内容是 COMPRESSION_SYSTEM_PROMPT，作为摘要指令
//! - 强制 `tools=[]`，独立于主 ReAct 流，不进对话流
//! - 多次压缩时旧 compaction 消息原样在序列里（role=system，content=旧摘要），
//!   LLM 自然能看到，不需要单独提取 previous_summary 注入

use crate::compressor::prompt::COMPRESSION_SYSTEM_PROMPT;
use crate::compressor::window::{estimate_tokens, select_recent};
use fuyao_api::{CompressionConfig, Message};
use fuyao_provider::{ChatMessage, ChatRequest, Provider, StreamError};

/// 压缩执行错误
#[derive(Debug, thiserror::Error)]
pub enum CompressionError {
    /// 摘要为空（LLM 没产出可用内容）
    #[error("摘要输出为空")]
    EmptySummary,
    /// LLM 调用失败
    #[error("LLM 调用失败: {0}")]
    LlmError(#[from] StreamError),
    /// 没有可压缩的内容（messages 为空或全在 tail）
    #[error("无可压缩内容")]
    NothingToCompress,
}

/// 摘要生成结果
#[derive(Debug, Clone)]
pub struct SummaryResult {
    /// 摘要正文（Markdown）
    pub text: String,
    /// 压缩前的 token 估算（反抖动统计用）
    pub tokens_before: usize,
    /// 压缩后的 token 估算（保留 tail + 摘要）
    pub tokens_after: usize,
}

/// 把 fuyao_api::Message 原样转成 provider 的 ChatMessage
///
/// 保留 role / content / reasoning / tool_calls / tool_call_id / tool_name 全部字段，
/// 不做任何序列化或重构——这是前缀缓存的生命线。
fn to_chat_message(m: &Message) -> ChatMessage {
    ChatMessage {
        role: m.role.clone(),
        content: m.content.clone(),
        reasoning: m.reasoning.clone(),
        tool_calls: m.tool_calls.as_ref().and_then(|tc| tc.as_array().cloned()),
        tool_call_id: m.tool_call_id.clone(),
        tool_name: m.tool_name.clone(),
    }
}

/// 生成摘要：消息原样发 + 末尾追加摘要指令
///
/// # 参数
/// - `system_prompt`：session 原本的 system_prompt（保持不变，前缀缓存命中）
/// - `messages`：当前 session 的可见消息（原样发，不构造、不序列化）
/// - `provider`：LLM provider（用 `chat()` 非流式接口）
/// - `model_id`：摘要用哪个模型（一般与主对话一致）
/// - `context_length`：模型上下文长度（用于按比例计算保留窗口预算）
/// - `cfg`：压缩配置
pub async fn generate_summary(
    system_prompt: Option<&str>,
    messages: &[Message],
    provider: &std::sync::Arc<dyn Provider>,
    model_id: &str,
    context_length: u32,
    cfg: &CompressionConfig,
) -> Result<SummaryResult, CompressionError> {
    if messages.len() < 2 {
        return Err(CompressionError::NothingToCompress);
    }

    // 窗口切分（仅用于估算 tokens_after / 决定 apply 时保留多少近账）
    // 保留预算按模型上下文比例动态计算
    let keep_tokens = cfg.effective_keep_tokens(context_length);
    let window = select_recent(messages, keep_tokens);
    if window.to_compress.is_empty() {
        return Err(CompressionError::NothingToCompress);
    }

    // 构造请求：消息原样 + 末尾追加摘要指令
    // system 保持 session 原值不变 —— 前缀缓存的生命线
    let mut chat_messages: Vec<ChatMessage> = messages.iter().map(to_chat_message).collect();
    chat_messages.push(ChatMessage {
        role: "user".to_string(),
        content: Some(COMPRESSION_SYSTEM_PROMPT.to_string()),
        ..Default::default()
    });

    let request = ChatRequest {
        messages: chat_messages,
        system: system_prompt.map(String::from),
    };

    // 调 provider（非流式 chat，不带 tools）
    let response = provider.chat(request, model_id).await?;

    let text = response.content.unwrap_or_default();
    let text = text.trim();
    if text.is_empty() {
        return Err(CompressionError::EmptySummary);
    }

    // 估算压缩效果（反抖动统计用）
    let tokens_before = estimate_tokens(messages);
    let summary_tokens = text.len().div_ceil(4);
    let tokens_after = estimate_tokens(window.keep_recent) + summary_tokens;

    Ok(SummaryResult {
        text: text.to_string(),
        tokens_before,
        tokens_after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use fuyao_provider::{BoxStream, ChatResponse, FinishReason, StreamUsage};
    use std::sync::Arc;

    /// 返回固定文本的 mock provider
    struct FixedProvider {
        content: String,
    }

    #[async_trait]
    impl Provider for FixedProvider {
        fn stream_chat(
            &self,
            _request: ChatRequest,
            _model: &str,
            _options: fuyao_provider::StreamOptions,
        ) -> BoxStream<Result<fuyao_provider::StreamEvent, StreamError>> {
            unimplemented!("压缩用 chat()")
        }

        async fn chat(
            &self,
            _request: ChatRequest,
            _model: &str,
        ) -> Result<ChatResponse, StreamError> {
            Ok(ChatResponse {
                content: Some(self.content.clone()),
                reasoning: None,
                tool_calls: None,
                usage: StreamUsage::default(),
                finish_reason: FinishReason::Stop,
            })
        }
    }

    fn cfg_small_keep() -> CompressionConfig {
        CompressionConfig {
            keep_ratio: 1.0,     // 比例拉满，让 effective_keep_tokens 永远等于 keep_tokens_max
            keep_tokens_max: 50, // 极小预算，强制压缩多数消息
            ..CompressionConfig::default()
        }
    }

    fn make_messages(n: usize) -> Vec<Message> {
        (0..n)
            .map(|i| Message::user(format!("消息_{i}_{}", "x".repeat(40))))
            .collect()
    }

    #[tokio::test]
    async fn generate_summary_returns_text() {
        let provider: Arc<dyn Provider> = Arc::new(FixedProvider {
            content: "## 目标\n- 测试".to_string(),
        });
        let msgs = make_messages(10);

        let result = generate_summary(
            Some("你是助手"),
            &msgs,
            &provider,
            "model",
            128_000,
            &cfg_small_keep(),
        )
        .await
        .unwrap();
        assert_eq!(result.text, "## 目标\n- 测试");
        assert!(result.tokens_before > 0);
    }

    #[tokio::test]
    async fn generate_summary_errors_on_empty() {
        let provider: Arc<dyn Provider> = Arc::new(FixedProvider {
            content: "   ".to_string(),
        });
        let msgs = make_messages(10);

        let result = generate_summary(
            Some("你是助手"),
            &msgs,
            &provider,
            "model",
            128_000,
            &cfg_small_keep(),
        )
        .await;
        assert!(matches!(result, Err(CompressionError::EmptySummary)));
    }

    #[tokio::test]
    async fn generate_summary_errors_when_nothing_to_compress() {
        let provider: Arc<dyn Provider> = Arc::new(FixedProvider {
            content: "x".to_string(),
        });
        let msgs = make_messages(1);
        let result = generate_summary(
            Some("你是助手"),
            &msgs,
            &provider,
            "model",
            128_000,
            &cfg_small_keep(),
        )
        .await;
        assert!(matches!(result, Err(CompressionError::NothingToCompress)));
    }

    #[test]
    fn to_chat_message_preserves_all_fields() {
        let mut msg = Message::assistant(Some("回复".to_string()));
        msg.reasoning = Some("思考".to_string());
        msg.tool_calls = Some(serde_json::json!([{"id": "call_1"}]));
        msg.tool_call_id = Some("call_1".to_string());
        msg.tool_name = Some("bash".to_string());

        let cm = to_chat_message(&msg);
        assert_eq!(cm.role, "assistant");
        assert_eq!(cm.content.as_deref(), Some("回复"));
        assert_eq!(cm.reasoning.as_deref(), Some("思考"));
        assert!(cm.tool_calls.is_some());
        assert_eq!(cm.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(cm.tool_name.as_deref(), Some("bash"));
    }
}
