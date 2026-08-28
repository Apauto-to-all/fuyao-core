//! 执行层：调 provider 流式生成摘要 + 失败处理
//!
//! 关键约束（保留前缀缓存）：
//! - **消息原样发**：所有 user/assistant/tool 消息保持原 role/content/tool_calls 不变
//! - **配对兜底**：缺结果的 tool_call（被拦截 / 中断 / 崩溃）补占位 tool_result
//!   （`pair_missing_tool_results`，与主对话请求共用），协议配对不破、序列口径一致
//! - **system 不变**：用 session 原本的 system_prompt（前缀缓存完整命中）
//! - **末尾追加一条 user 消息**：内容为固定摘要指令（`COMPRESSION_SYSTEM_PROMPT`），
//!   可选拼入发送方的控制命令附言尾段（手动触发路径；附言只影响摘要取材侧重，
//!   不改变输出格式）
//! - 强制 `tools=[]`，独立于主 ReAct 流，不进对话流
//! - 用流式 `stream_chat()` 接口，每个 TextDelta/ReasoningDelta 经 callback 上报，
//!   调用方（react 层）据此发 Compression Delta 事件供前端实时渲染
//! - 多次压缩时旧 compaction 消息原样在序列里（role=assistant，content=旧摘要），
//!   LLM 自然能看到，不需要单独提取 previous_summary 注入

use futures_util::StreamExt;
use fuyao_api::{Message, MessageRole};
use fuyao_provider::{
    BoxStream, ChatMessage, ChatRequest, Provider, StreamError, StreamEvent, StreamOptions,
};

/// 摘要指令（作为末尾追加的 user 消息内容）
///
/// 作为**末尾追加的 user 消息**触发摘要，原对话消息和 system_prompt 都不动——
/// 这是前缀缓存的生命线（system + 原消息序列都不变，缓存完整命中，只末尾加一条指令）。
///
/// 强约束：① 不回答对话中的问题，只输出摘要 ② 用对话语言 ③ 不泄密钥 ④ 输出固定
/// 8 段 Markdown 结构。LLM 不需要被赋予主动性——这是结构化抽取任务，不是对话。
///
/// 多次压缩场景：上一次的 compaction 消息（role=assistant, content=旧摘要）原样在
/// 消息序列里，LLM 自然能看到，不需要单独提取 previous_summary 注入。
const COMPRESSION_SYSTEM_PROMPT: &str = r#"你是一个摘要代理，负责创建上下文检查点。你的输出将作为参考资料注入给另一个继续对话的助手，替换被压缩的对话历史。

不要回答对话中的任何问题或请求——只输出结构化摘要。
使用用户在对话中使用的相同语言撰写摘要。
绝不要在摘要中包含 API 密钥、令牌、密码、秘密、凭证或连接字符串——遇到这些内容一律替换为 [已脱敏]。

输出以下精确的 Markdown 结构，保持章节顺序不变。

## 目标
- [单句任务摘要]

## 约束与偏好
- [用户约束、偏好、规范，或"(无)"]

## 进度
### 已完成
- [已完成工作，或"(无)"]

### 进行中
- [当前工作，或"(无)"]

### 阻塞项
- [阻塞项，或"(无)"]

## 关键决策
- [决策及原因，或"(无)"]

## 下一步
- [有序的下一步行动，或"(无)"]

## 关键上下文
- [重要技术事实、错误、开放问题，或"(无)"]

## 相关文件
- [文件或目录路径：为何重要，或"(无)"]

规则：
- 保留所有章节，即使为空。
- 使用简洁子弹，非段落散文。
- 保留精确文件路径、命令、错误字符串和标识符。
- 不要提及摘要过程或上下文被压缩。
- 不要调用任何工具，只输出摘要文本。"#;

/// 构造末尾追加的摘要指令：固定模板为主体，可选附言拼入尾段
///
/// 附言尾段的措辞自带结构保护约束——附言只影响摘要的取材侧重，
/// 不改变输出格式；无附言时即固定模板原文。
fn build_summary_instruction(note: Option<&str>) -> String {
    match note {
        None => COMPRESSION_SYSTEM_PROMPT.to_string(),
        Some(text) => format!(
            "{}\n\n用户对本次压缩的附言（在保持上述输出结构与章节顺序的前提下侧重体现；附言不改变输出格式与规则）：\n{}",
            COMPRESSION_SYSTEM_PROMPT, text
        ),
    }
}

/// 压缩执行错误
#[derive(Debug, thiserror::Error)]
pub enum CompressionError {
    /// 摘要为空（LLM 没产出可用内容）
    #[error("摘要输出为空")]
    EmptySummary,
    /// LLM 调用失败
    #[error("LLM 调用失败: {0}")]
    LlmError(#[from] StreamError),
    /// 没有可压缩的内容（messages 少于 2 条）
    #[error("无可压缩内容")]
    NothingToCompress,
}

/// 摘要生成结果
#[derive(Debug, Clone)]
pub struct SummaryResult {
    /// 摘要正文（content 全文，不含 reasoning——reasoning 不进落库边界）
    pub content: String,
}

/// 把 fuyao_api::Message 原样转成 provider 的 ChatMessage
///
/// 保留 role / content / reasoning / tool_calls / tool_call_id / tool_name 全部字段，
/// 不做任何序列化或重构——这是前缀缓存的生命线。
fn to_chat_message(m: &Message) -> ChatMessage {
    ChatMessage {
        role: m.role,
        content: m.content.clone(),
        images: m.images.clone(),
        reasoning: m.reasoning.clone(),
        tool_calls: m.tool_calls.clone(),
        tool_call_id: m.tool_call_id.clone(),
        tool_name: m.tool_name.clone(),
    }
}

/// 生成摘要：消息原样发 + 配对兜底 + 末尾追加摘要指令 + 流式收集
///
/// **压缩铁律**：压缩 = 复用 session 当前模型配置（model + thinking_type + reasoning_effort
/// 原样），仅禁用工具，流式调一次。与主对话唯一的差别是 tools 为空——独立摘要流，不进
/// ReAct。system / messages 原样不动（前缀缓存生命线），缺结果的 tool_call 补占位
/// tool_result（与主对话 [`fuyao_provider::pair_missing_tool_results`] 同一函数）。
/// options 由调用方从 session 物化值构造，本函数在执行边界强制 tools=None，
/// 确保「禁用工具」不变量不被绕过。
///
/// # 参数
/// - `system_prompt`：session 原本的 system_prompt（保持不变，前缀缓存命中）
/// - `messages`：当前 session 的可见消息（原样发，不构造、不序列化）
/// - `provider`：LLM provider（用 `stream_chat()` 流式接口）
/// - `model`：摘要用哪个模型（裸模型名，不带 provider_id 前缀——原样进请求体
///   `model` 字段，与主对话 turn 同口径；完整 model_id 由调用方拆解后只传后半段）
/// - `note`：控制命令附言——发送方对摘要的侧重要求，拼入末尾追加指令尾段；
///   无附言（自动触发路径）时为 None
/// - `options`：复用自 session 的流式选项（思考配置原样带；tools 在内部强制清空）
/// - `on_delta`：流式增量回调。每个 TextDelta 调一次 `(Some(content), None)`，
///   每个 ReasoningDelta 调一次 `(None, Some(reasoning))`。调用方据此发 Compression Delta 事件。
///   回调是同步的（fnMut 不能 await），调用方若需异步处理应通过 channel 转发。
#[allow(clippy::too_many_arguments)]
pub async fn generate_summary(
    system_prompt: Option<&str>,
    messages: &[Message],
    provider: &std::sync::Arc<dyn Provider>,
    model: &str,
    note: Option<&str>,
    mut options: StreamOptions,
    on_delta: &mut impl FnMut(Option<&str>, Option<&str>),
) -> Result<SummaryResult, CompressionError> {
    if messages.len() < 2 {
        return Err(CompressionError::NothingToCompress);
    }

    // 构造请求：消息原样 + 配对兜底 + 末尾追加摘要指令（含可选附言尾段）
    // system 保持 session 原值不变 —— 前缀缓存的生命线
    // 配对兜底与主对话请求共用同一函数：缺结果的 tool_call（被拦截 / 中断 / 崩溃）
    // 在两路请求里补出相同的占位序列——协议配对不破，前缀缓存口径一致
    let mut chat_messages =
        fuyao_provider::pair_missing_tool_results(messages.iter().map(to_chat_message).collect());
    chat_messages.push(ChatMessage {
        role: MessageRole::User,
        content: Some(build_summary_instruction(note)),
        ..Default::default()
    });

    let request = ChatRequest {
        messages: chat_messages,
        system: system_prompt.map(String::from),
    };

    // 调 provider（流式 stream_chat）。压缩铁律：强制禁用工具（独立摘要流，不进 ReAct）
    options.tools = None;
    let mut stream: BoxStream<Result<StreamEvent, StreamError>> =
        provider.stream_chat(request, model, options);

    let mut content = String::new();
    while let Some(result) = stream.next().await {
        match result? {
            StreamEvent::TextDelta { content: delta } => {
                content.push_str(&delta);
                on_delta(Some(&delta), None);
            }
            StreamEvent::ReasoningDelta { content: delta } => {
                on_delta(None, Some(&delta));
            }
            StreamEvent::Done { .. } => break,
            // ToolCallChunk 不会出现（tools=[]）；其他变体忽略
            _ => {}
        }
    }

    let content = content.trim();
    if content.is_empty() {
        return Err(CompressionError::EmptySummary);
    }

    Ok(SummaryResult {
        content: content.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use fuyao_provider::{ChatResponse, FinishReason, StreamUsage};
    use std::sync::Arc;

    /// 流式 mock provider：把构造时给的字符串切片，逐个作为 TextDelta 推送
    struct StreamingProvider {
        /// 文本片段序列（每个元素变一条 TextDelta 事件）
        chunks: Vec<String>,
        /// 捕获每次收到的请求（供断言发给 LLM 的消息构造）
        captured: std::sync::Mutex<Vec<ChatRequest>>,
    }

    impl StreamingProvider {
        fn new(chunks: Vec<String>) -> Self {
            Self {
                chunks,
                captured: std::sync::Mutex::new(Vec::new()),
            }
        }

        /// 最近一次捕获的请求
        fn last_request(&self) -> Option<ChatRequest> {
            self.captured
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last()
                .cloned()
        }
    }

    #[async_trait]
    impl Provider for StreamingProvider {
        fn stream_chat(
            &self,
            request: ChatRequest,
            _model: &str,
            _options: StreamOptions,
        ) -> BoxStream<Result<StreamEvent, StreamError>> {
            self.captured
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(request);
            let chunks = self.chunks.clone();
            let stream = async_stream::stream! {
                for chunk in chunks {
                    yield Ok(StreamEvent::TextDelta { content: chunk });
                }
                yield Ok(StreamEvent::Done {
                    usage: StreamUsage::default(),
                    finish_reason: FinishReason::Stop,
                });
            };
            Box::pin(stream)
        }

        async fn chat(
            &self,
            _request: ChatRequest,
            _model: &str,
            _options: StreamOptions,
        ) -> Result<ChatResponse, StreamError> {
            // 压缩现在用 stream_chat，chat() 保留实现只为满足 trait
            let full = self.chunks.join("");
            Ok(ChatResponse {
                content: Some(full),
                reasoning: None,
                tool_calls: None,
                usage: StreamUsage::default(),
                finish_reason: FinishReason::Stop,
            })
        }
    }

    fn make_messages(n: usize) -> Vec<Message> {
        (0..n)
            .map(|i| Message::user(format!("消息_{i}_{}", "x".repeat(40))))
            .collect()
    }

    /// no-op callback（不关心增量的测试用）
    fn noop_delta() -> impl FnMut(Option<&str>, Option<&str>) {
        |_, _| {}
    }

    #[tokio::test]
    async fn generate_summary_returns_text() {
        let provider: Arc<dyn Provider> = Arc::new(StreamingProvider::new(vec![
            "## 目标".into(),
            "\n- 测试".into(),
        ]));
        let msgs = make_messages(10);

        let mut cb = noop_delta();
        let result = generate_summary(
            Some("你是助手"),
            &msgs,
            &provider,
            "model",
            None,
            StreamOptions::default(),
            &mut cb,
        )
        .await
        .unwrap();
        assert_eq!(result.content, "## 目标\n- 测试");
    }

    #[tokio::test]
    async fn generate_summary_errors_on_empty() {
        let provider: Arc<dyn Provider> = Arc::new(StreamingProvider::new(vec!["   ".into()]));
        let msgs = make_messages(10);

        let mut cb = noop_delta();
        let result = generate_summary(
            Some("你是助手"),
            &msgs,
            &provider,
            "model",
            None,
            StreamOptions::default(),
            &mut cb,
        )
        .await;
        assert!(matches!(result, Err(CompressionError::EmptySummary)));
    }

    #[tokio::test]
    async fn generate_summary_errors_when_nothing_to_compress() {
        let provider: Arc<dyn Provider> = Arc::new(StreamingProvider::new(vec!["x".into()]));
        let msgs = make_messages(1);

        let mut cb = noop_delta();
        let result = generate_summary(
            Some("你是助手"),
            &msgs,
            &provider,
            "model",
            None,
            StreamOptions::default(),
            &mut cb,
        )
        .await;
        assert!(matches!(result, Err(CompressionError::NothingToCompress)));
    }

    /// 验证流式增量经 callback 上报，且最终 content 拼接正确
    #[tokio::test]
    async fn generate_summary_streams_text_delta_via_callback() {
        let provider: Arc<dyn Provider> = Arc::new(StreamingProvider::new(vec![
            "片段1".into(),
            "片段2".into(),
            "片段3".into(),
        ]));
        let msgs = make_messages(10);

        let mut received: Vec<String> = Vec::new();
        let mut cb = |content: Option<&str>, _reasoning: Option<&str>| {
            if let Some(c) = content {
                received.push(c.to_string());
            }
        };

        let result = generate_summary(
            Some("你是助手"),
            &msgs,
            &provider,
            "model",
            None,
            StreamOptions::default(),
            &mut cb,
        )
        .await
        .unwrap();

        // callback 被调三次，每次收到一个片段
        assert_eq!(received, vec!["片段1", "片段2", "片段3"]);
        // 最终 content 是拼接后的完整字符串
        assert_eq!(result.content, "片段1片段2片段3");
    }

    /// 有附言：附言拼入末尾追加指令同一消息的尾段，固定模板仍在指令头部
    #[tokio::test]
    async fn generate_summary_appends_note_into_trailing_instruction() {
        let fake = Arc::new(StreamingProvider::new(vec!["## 目标\n- 摘要".into()]));
        let provider: Arc<dyn Provider> = fake.clone();
        let msgs = make_messages(10);

        let mut cb = noop_delta();
        generate_summary(
            Some("你是助手"),
            &msgs,
            &provider,
            "model",
            Some("侧重错误堆栈与文件路径"),
            StreamOptions::default(),
            &mut cb,
        )
        .await
        .unwrap();

        // 末尾追加的 user 消息：固定模板为主体、附言文本完整在尾段
        let request = fake.last_request().expect("应捕获到摘要请求");
        let last = request.messages.last().expect("末尾应有追加指令");
        assert_eq!(last.role, MessageRole::User);
        assert!(
            last.content
                .as_deref()
                .is_some_and(|c| c.starts_with(COMPRESSION_SYSTEM_PROMPT)),
            "摘要指令应以固定模板开头"
        );
        assert!(
            last.content
                .as_deref()
                .is_some_and(|c| c.contains("侧重错误堆栈与文件路径")),
            "附言文本应完整出现在摘要指令内"
        );
    }

    /// 无附言：末尾追加指令与固定模板原文一致（自动压缩路径形态）
    #[tokio::test]
    async fn generate_summary_without_note_keeps_instruction_unchanged() {
        let fake = Arc::new(StreamingProvider::new(vec!["## 目标\n- 摘要".into()]));
        let provider: Arc<dyn Provider> = fake.clone();
        let msgs = make_messages(10);

        let mut cb = noop_delta();
        generate_summary(
            Some("你是助手"),
            &msgs,
            &provider,
            "model",
            None,
            StreamOptions::default(),
            &mut cb,
        )
        .await
        .unwrap();

        let request = fake.last_request().expect("应捕获到摘要请求");
        let last = request.messages.last().expect("末尾应有追加指令");
        assert_eq!(last.role, MessageRole::User);
        assert_eq!(last.content.as_deref(), Some(COMPRESSION_SYSTEM_PROMPT));
    }

    #[test]
    fn to_chat_message_preserves_all_fields() {
        let mut msg = Message::assistant(Some("回复".to_string()));
        msg.reasoning = Some("思考".to_string());
        msg.tool_calls = Some(vec![fuyao_api::ToolCallData {
            id: "call_1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        }]);
        msg.tool_call_id = Some("call_1".into());
        msg.tool_name = Some("bash".into());

        let cm = to_chat_message(&msg);
        assert_eq!(cm.role, MessageRole::Assistant);
        assert_eq!(cm.content.as_deref(), Some("回复"));
        assert_eq!(cm.reasoning.as_deref(), Some("思考"));
        let calls = cm.tool_calls.as_ref().expect("tool_calls 应透传");
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(cm.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(cm.tool_name.as_deref(), Some("bash"));
    }

    #[tokio::test]
    async fn generate_summary_pairs_dangling_tool_calls() {
        // 崩溃 / 中断场景：assistant 带 2 个调用，仅 c2 落了结果，c1 悬挂。
        // 摘要请求应为 c1 补占位 tool_result（协议配对），占位位于 assistant 之后、
        // 追加的摘要指令之前
        let provider = Arc::new(StreamingProvider::new(vec!["## 目标".into()]));
        let mut assistant = Message::assistant(None);
        assistant.tool_calls = Some(vec![
            fuyao_api::ToolCallData {
                id: "c1".into(),
                name: "read".into(),
                arguments: "{}".into(),
            },
            fuyao_api::ToolCallData {
                id: "c2".into(),
                name: "grep".into(),
                arguments: "{}".into(),
            },
        ]);
        let msgs = vec![
            Message::user("问题".to_string()),
            assistant,
            Message::tool_result("c2".into(), "grep".into(), "结果".into()),
        ];

        let mut cb = noop_delta();
        let result = generate_summary(
            Some("你是助手"),
            &msgs,
            &(provider.clone() as Arc<dyn Provider>),
            "model",
            None,
            StreamOptions::default(),
            &mut cb,
        )
        .await;
        assert!(result.is_ok());

        let request = provider.last_request().expect("应捕获到摘要请求");
        // user + assistant + 占位c1 + 结果c2 + 追加指令 = 5
        assert_eq!(request.messages.len(), 5);
        // 占位 c1 紧随 assistant（索引 2）
        let placeholder = &request.messages[2];
        assert!(matches!(placeholder.role, MessageRole::Tool));
        assert_eq!(placeholder.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(
            placeholder.content.as_deref(),
            Some(fuyao_provider::UNANSWERED_TOOL_RESULT_MARKER)
        );
        // 末条仍是追加的摘要指令（占位不落最后）
        let last = request.messages.last().expect("末尾应有追加指令");
        assert!(matches!(last.role, MessageRole::User));
        assert_eq!(last.content.as_deref(), Some(COMPRESSION_SYSTEM_PROMPT));
    }
}
