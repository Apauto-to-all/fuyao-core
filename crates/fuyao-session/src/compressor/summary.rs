//! 执行层：构造 prompt + 调 provider 生成摘要 + 失败处理
//!
//! 关键约束（对齐 opencode + hermes 共识）：
//! - 摘要 LLM 调用强制 `tools=[]`（不传 tools 字段），独立于主 ReAct 流
//! - 摘要 LLM 输出**不进对话流**——结果直接进 summary 字段，不存在识别问题
//! - 不做显式摘要质量验证（三项目共识），靠结构化 prompt + 反抖动间接保证

use crate::compressor::prompt::{COMPRESSION_SYSTEM_PROMPT, build_prompt};
use crate::compressor::window::{estimate_tokens, select_recent, serialize_for_summary};
use fuyao_api::{CompressionConfig, Message, MessageKind};
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
    /// 上一次的摘要（apply 层 mark_compaction 后会更新到 sessions 表，
    /// 下次压缩时从 messages 数组里查找历史 compaction 消息重新提取）
    pub previous_summary: Option<String>,
}

/// 生成摘要：调一次 LLM（`tools=[]`）把旧消息压成结构化 Markdown
///
/// # 参数
/// - `messages`：当前 session 的可见消息（已不含被压过的旧消息）
/// - `provider`：LLM provider（用 `chat()` 非流式接口）
/// - `model_id`：摘要用哪个模型（一般与主对话一致）
/// - `cfg`：压缩配置
pub async fn generate_summary(
    messages: &[Message],
    provider: &std::sync::Arc<dyn Provider>,
    model_id: &str,
    cfg: &CompressionConfig,
) -> Result<SummaryResult, CompressionError> {
    if messages.len() < 2 {
        return Err(CompressionError::NothingToCompress);
    }

    // 窗口切分
    let window = select_recent(messages, cfg.keep_tokens);
    if window.to_compress.is_empty() {
        return Err(CompressionError::NothingToCompress);
    }

    // 从 to_compress 里找历史 compaction 摘要（多次压缩增量更新）
    let previous_summary = window
        .to_compress
        .iter()
        .rev()
        .find(|m| m.kind == MessageKind::Compaction)
        .and_then(|m| m.content.clone());

    // 序列化旧消息为喂 LLM 的纯文本
    let serialized = serialize_for_summary(window.to_compress);
    let user_content = build_prompt(&serialized, previous_summary.as_deref());

    // 构造请求：单条 user 消息 + system prompt，不带 tools
    let request = ChatRequest {
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: Some(user_content),
            ..Default::default()
        }],
        system: Some(COMPRESSION_SYSTEM_PROMPT.to_string()),
    };

    // 调 provider（非流式 chat）
    let response = provider.chat(request, model_id).await?;

    let text = response.content.unwrap_or_default();
    let text = text.trim();
    if text.is_empty() {
        return Err(CompressionError::EmptySummary);
    }

    // 估算压缩效果（反抖动统计用）
    let tokens_before = estimate_tokens(window.to_compress);
    let summary_tokens = text.len().div_ceil(4);
    let tokens_after = estimate_tokens(window.keep_recent) + summary_tokens;

    Ok(SummaryResult {
        text: text.to_string(),
        tokens_before: tokens_before as usize,
        tokens_after,
        previous_summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use fuyao_provider::{BoxStream, ChatResponse, FinishReason, StreamEvent, StreamUsage};
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
        ) -> BoxStream<Result<StreamEvent, StreamError>> {
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

    fn cfg() -> CompressionConfig {
        CompressionConfig::default()
    }

    fn cfg_small_keep() -> CompressionConfig {
        CompressionConfig {
            keep_tokens: 50, // 极小预算，强制压缩多数消息
            ..CompressionConfig::default()
        }
    }

    #[tokio::test]
    async fn generate_summary_returns_text() {
        let provider: Arc<dyn Provider> = Arc::new(FixedProvider {
            content: "## 目标\n- 测试".to_string(),
        });
        // 每条 ~16 token × 10 条 = 160 token，远超 keep_tokens=50
        let msgs: Vec<Message> = (0..10)
            .map(|i| Message::user(format!("消息_{i}_{}", "x".repeat(40))))
            .collect();

        let result = generate_summary(&msgs, &provider, "model", &cfg_small_keep())
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
        let msgs: Vec<Message> = (0..10)
            .map(|i| Message::user(format!("消息_{i}_{}", "x".repeat(40))))
            .collect();

        let result = generate_summary(&msgs, &provider, "model", &cfg_small_keep()).await;
        assert!(matches!(result, Err(CompressionError::EmptySummary)));
    }

    #[tokio::test]
    async fn generate_summary_errors_when_nothing_to_compress() {
        let provider: Arc<dyn Provider> = Arc::new(FixedProvider {
            content: "x".to_string(),
        });
        // 只有 1 条消息
        let msgs = vec![Message::user("唯一消息".to_string())];
        let result = generate_summary(&msgs, &provider, "model", &cfg_small_keep()).await;
        assert!(matches!(result, Err(CompressionError::NothingToCompress)));
    }
}
