//! 单次 LLM 流式调用会话
//!
//! 封装 Provider 流式调用的完整生命周期：
//! - 创建 stream → 重试重建（退避）
//! - 解码事件 → 累积 text/reasoning/tool_calls
//! - 推送流式事件到前端
//! - 返回完整结果

use crate::engine::EventEmitter;
use crate::interrupt::{StreamAccumulator, StreamPhase};
use futures_util::{StreamExt, pin_mut};
use fuyao_api::SharedAgentCtx;
use fuyao_api::message::output::{ErrorMessage, ErrorPayload, ToolCallMessage, ToolCallPayload};
use fuyao_api::message::{EventBase, OutputEvent};
use fuyao_hooks::LlmErrorAction;
use fuyao_provider::{
    ChatRequest, Provider as LlmProvider, StreamDecoder, StreamError, StreamOptions, StreamUsage,
    ToolCallData as ProviderToolCallData,
};
use fuyao_provider::{backoff_duration, is_retryable};
use std::sync::{Arc, Mutex};

/// 流式会话最终结果
pub struct StreamResult {
    /// 累积的文本内容
    pub text: String,
    /// 累积的推理内容
    pub reasoning: String,
    /// 解析出的工具调用列表
    pub tool_calls: Vec<ProviderToolCallData>,
    /// Token 用量统计
    pub usage: StreamUsage,
}

/// 运行一次完整的流式 LLM 调用（含重试/退避）
///
/// # 参数
/// - `provider`: LLM Provider 实例
/// - `request`: 聊天请求（消息列表等）
/// - `agent_ctx`: Agent 上下文（含 model_id）
/// - `tools_schema`: 工具 schema 列表
/// - `emitter`: 统一事件发送器
/// - `accumulator`: 共享累积器（用于中断时读取部分结果）
///
/// # 返回
/// - `Ok(StreamResult)`: 成功完成，含累积内容、工具调用、用量
/// - `Err(())`: 不可恢复错误
pub async fn run_stream_session(
    provider: &dyn LlmProvider,
    request: ChatRequest,
    agent_ctx: &SharedAgentCtx,
    tools_schema: Option<Vec<serde_json::Value>>,
    emitter: &EventEmitter,
    accumulator: Option<Arc<Mutex<StreamAccumulator>>>,
) -> Result<StreamResult, StreamError> {
    let mut decoder = StreamDecoder::new();
    let mut retry_count = 0u32;
    let mut accumulated_text = String::new();
    let mut accumulated_reasoning = String::new();
    let started = std::time::Instant::now();

    'stream: loop {
        // 一次锁取全 model_config（AgentContext 是 Arc<Mutex>，禁止重复加锁）
        let (current_model_id, thinking_type, reasoning_effort) = {
            let g = agent_ctx.lock().expect("Agent 上下文锁异常");
            (
                g.model_config
                    .model_id
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                g.model_config.thinking_type.clone(),
                g.model_config.reasoning_effort.clone(),
            )
        };
        // API 请求使用短名（"aliyun/qwen3.6-plus" → "qwen3.6-plus"）
        let api_model_name = current_model_id
            .split('/')
            .nth(1)
            .unwrap_or(&current_model_id);

        let options = StreamOptions {
            temperature: None,
            tools: tools_schema.clone(),
            tool_choice: None,
            thinking_type,
            reasoning_effort,
        };

        let stream = provider.stream_chat(request.clone(), api_model_name, options);
        pin_mut!(stream);

        while let Some(result) = stream.next().await {
            match result {
                Ok(event) => {
                    retry_count = 0;
                    let output_events = decoder.process(event);
                    for oe in output_events {
                        if let OutputEvent::Chunk(msg) = &oe {
                            if let Some(content) = &msg.payload.content {
                                accumulated_text.push_str(content);
                            }
                            if let Some(reasoning) = &msg.payload.reasoning {
                                accumulated_reasoning.push_str(reasoning);
                            }
                        }

                        // 统一事件发送：通过 dispatch 管道
                        crate::dispatch::dispatch(oe, None, emitter).await;
                    }

                    // 同步共享累积器（供中断时读取部分结果）
                    if let Some(acc) = &accumulator {
                        let mut guard = acc.lock().expect("流式累积器锁异常");
                        guard.text = accumulated_text.clone();
                        guard.reasoning = accumulated_reasoning.clone();
                        guard.tool_calls = decoder.peek_tool_calls();
                        guard.usage = decoder.usage().clone();
                    }
                }
                Err(e) if !is_retryable(&e) => {
                    tracing::warn!(reason = %e, "LLM 请求不可恢复失败");
                    crate::dispatch::dispatch(
                        OutputEvent::Error(ErrorMessage {
                            base: EventBase::default(),
                            payload: ErrorPayload {
                                message: e.to_string(),
                                recoverable: false,
                            },
                        }),
                        None,
                        emitter,
                    )
                    .await;
                    return Err(e);
                }
                Err(e) => {
                    retry_count += 1;
                    let backoff = backoff_duration(retry_count, &e);
                    tracing::warn!(
                        attempt = retry_count,
                        reason = %e,
                        retry_after_ms = backoff.as_millis() as u64,
                        "LLM 请求重试"
                    );

                    crate::dispatch::dispatch(
                        OutputEvent::Error(ErrorMessage {
                            base: EventBase::default(),
                            payload: ErrorPayload {
                                message: format!("LLM 错误 (重试 {}): {}", retry_count, e),
                                recoverable: true,
                            },
                        }),
                        None,
                        emitter,
                    )
                    .await;

                    let action = {
                        let h = emitter.hooks().lock().await;
                        h.hook_on_llm_error(&e.to_string(), retry_count)
                    };

                    match action {
                        LlmErrorAction::Retry => {
                            // 标记退避阶段（中断时区分场景）
                            if let Some(acc) = &accumulator {
                                acc.lock().expect("流式累积器锁异常").phase = StreamPhase::Backoff;
                            }
                            tokio::time::sleep(backoff).await;
                            // 恢复流式阶段
                            if let Some(acc) = &accumulator {
                                acc.lock().expect("流式累积器锁异常").phase =
                                    StreamPhase::Streaming;
                            }
                            continue 'stream;
                        }
                        LlmErrorAction::Abort => {
                            tracing::warn!(
                                attempt = retry_count,
                                reason = %e,
                                "LLM 请求被钩子中止"
                            );
                            crate::dispatch::dispatch(
                                OutputEvent::Error(ErrorMessage {
                                    base: EventBase::default(),
                                    payload: ErrorPayload {
                                        message: e.to_string(),
                                        recoverable: false,
                                    },
                                }),
                                None,
                                emitter,
                            )
                            .await;
                            return Err(e);
                        }
                    }
                }
            }
        }

        break;
    }

    // 流结束后，从 decoder 取出工具调用，逐个经拦截管道生成"最终消息"
    //
    // 工具调用消息经过 output_intercept 拦截后，拦截结果（可能被插件修改参数）
    // 作为"最终消息"统一用于下游消费：CLI 显示、AssistantMessage 存储、工具执行。
    // 被 Block 的工具调用跳过（不执行、不存储），保证三者数据一致。
    let raw_tool_calls = decoder.take_tool_calls();
    let mut effective_tool_calls = Vec::with_capacity(raw_tool_calls.len());
    for tc in &raw_tool_calls {
        let args: serde_json::Value = match serde_json::from_str(&tc.arguments) {
            Ok(v) => v,
            Err(_) => {
                let raw: String = tc.arguments.chars().take(200).collect();
                tracing::warn!(tool = %tc.name, raw = %raw, "工具参数 JSON 解析失败");
                serde_json::Value::Null
            }
        };
        let event = OutputEvent::ToolCall(ToolCallMessage {
            base: EventBase::default(),
            payload: ToolCallPayload {
                tool_call_id: tc.id.clone(),
                tool_name: tc.name.clone(),
                tool_args: args,
            },
        });

        // 拦截 → 捕获最终消息 → 发送（CLI 渲染 + 观察钩子）
        match crate::dispatch::dispatch_intercept(event, None, emitter).await {
            Some(intercepted) => {
                // 从拦截后的最终消息提取工具调用数据，供执行与存储使用
                if let OutputEvent::ToolCall(msg) = &intercepted {
                    effective_tool_calls.push(ProviderToolCallData {
                        id: msg.payload.tool_call_id.clone(),
                        name: msg.payload.tool_name.clone(),
                        arguments: msg.payload.tool_args.to_string(),
                    });
                }
                crate::dispatch::deliver(emitter, intercepted).await;
            }
            None => {
                // 被 Block：跳过此工具调用（不执行、不存储）
            }
        }
    }

    let usage = decoder.usage().clone();
    let model_id = {
        let g = agent_ctx.lock().expect("Agent 上下文锁异常");
        g.model_config
            .model_id
            .clone()
            .unwrap_or_else(|| "unknown".to_string())
    };
    tracing::info!(
        model = %model_id,
        elapsed_ms = started.elapsed().as_millis() as u64,
        tokens_in = usage.prompt_tokens,
        tokens_out = usage.completion_tokens,
        thinking = usage.completion_reasoning_tokens.unwrap_or(0) > 0,
        "LLM 请求完成"
    );

    Ok(StreamResult {
        text: accumulated_text,
        reasoning: accumulated_reasoning,
        tool_calls: effective_tool_calls,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures_util::stream;
    use fuyao_api::AgentContext;
    use fuyao_hooks::{HooksRegistry, InterceptResult, SharedHooks};
    use fuyao_provider::{BoxStream, ChatResponse, FinishReason, Provider, StreamEvent};
    use tokio::sync::mpsc;

    /// Mock Provider：返回预设的 StreamEvent 序列
    struct MockProvider {
        events: Vec<StreamEvent>,
    }

    #[async_trait]
    impl Provider for MockProvider {
        fn stream_chat(
            &self,
            _request: ChatRequest,
            _model: &str,
            _options: StreamOptions,
        ) -> BoxStream<Result<StreamEvent, StreamError>> {
            let events: Vec<Result<StreamEvent, StreamError>> =
                self.events.clone().into_iter().map(Ok).collect();
            Box::pin(stream::iter(events))
        }

        async fn chat(
            &self,
            _request: ChatRequest,
            _model: &str,
        ) -> Result<ChatResponse, StreamError> {
            Err(StreamError::ApiError("mock: chat 不支持".into()))
        }
    }

    /// 构造含单个工具调用（git status）的事件序列
    fn tool_call_events() -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolCallChunk {
                index: 0,
                id: Some("tc1".into()),
                name: Some("git".into()),
                args_delta: Some(r#"{"command":"status"}"#.into()),
            },
            StreamEvent::Done {
                usage: StreamUsage::default(),
                finish_reason: FinishReason::ToolCalls,
            },
        ]
    }

    /// 构造 EventEmitter + 后台 drain（防止 channel 满阻塞 send）
    fn make_emitter(hooks: SharedHooks) -> EventEmitter {
        let (tx, mut rx) = mpsc::channel(128);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        EventEmitter::new(tx, hooks)
    }

    /// 空 hooks（无拦截钩子）
    fn empty_hooks() -> SharedHooks {
        Arc::new(tokio::sync::Mutex::new(HooksRegistry::new()))
    }

    /// 无拦截钩子：StreamResult 保持 LLM 原始工具调用
    #[tokio::test]
    async fn no_intercept_passes_original() {
        let provider = MockProvider {
            events: tool_call_events(),
        };
        let agent_ctx = Arc::new(std::sync::Mutex::new(AgentContext::default()));
        let emitter = make_emitter(empty_hooks());

        let result = run_stream_session(
            &provider,
            ChatRequest::default(),
            &agent_ctx,
            None,
            &emitter,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "git");
        assert_eq!(result.tool_calls[0].id, "tc1");
    }

    /// 拦截修改工具名：StreamResult 使用修改后的值
    #[tokio::test]
    async fn intercept_modifies_tool_name() {
        let hooks = empty_hooks();
        {
            let mut h = hooks.lock().await;
            h.register_output_intercept(
                0,
                Arc::new(|event: &OutputEvent| {
                    if let OutputEvent::ToolCall(msg) = event {
                        let mut modified = msg.clone();
                        modified.payload.tool_name = "rtk git".to_string();
                        InterceptResult::Pass(OutputEvent::ToolCall(modified))
                    } else {
                        InterceptResult::Pass(event.clone())
                    }
                }),
            );
        }

        let provider = MockProvider {
            events: tool_call_events(),
        };
        let agent_ctx = Arc::new(std::sync::Mutex::new(AgentContext::default()));
        let emitter = make_emitter(hooks);

        let result = run_stream_session(
            &provider,
            ChatRequest::default(),
            &agent_ctx,
            None,
            &emitter,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "rtk git");
    }

    /// 拦截阻止工具调用：StreamResult 中不包含被阻止的调用
    #[tokio::test]
    async fn intercept_blocks_tool_call() {
        let hooks = empty_hooks();
        {
            let mut h = hooks.lock().await;
            h.register_output_intercept(
                0,
                Arc::new(|event: &OutputEvent| {
                    if let OutputEvent::ToolCall(_) = event {
                        InterceptResult::Block("测试阻止".to_string())
                    } else {
                        InterceptResult::Pass(event.clone())
                    }
                }),
            );
        }

        let provider = MockProvider {
            events: tool_call_events(),
        };
        let agent_ctx = Arc::new(std::sync::Mutex::new(AgentContext::default()));
        let emitter = make_emitter(hooks);

        let result = run_stream_session(
            &provider,
            ChatRequest::default(),
            &agent_ctx,
            None,
            &emitter,
            None,
        )
        .await
        .unwrap();

        assert!(result.tool_calls.is_empty());
    }

    /// 拦截修改工具参数：StreamResult 使用修改后的参数
    #[tokio::test]
    async fn intercept_modifies_tool_args() {
        let hooks = empty_hooks();
        {
            let mut h = hooks.lock().await;
            h.register_output_intercept(
                0,
                Arc::new(|event: &OutputEvent| {
                    if let OutputEvent::ToolCall(msg) = event {
                        let mut modified = msg.clone();
                        modified.payload.tool_args =
                            serde_json::json!({"command": "log --oneline"});
                        InterceptResult::Pass(OutputEvent::ToolCall(modified))
                    } else {
                        InterceptResult::Pass(event.clone())
                    }
                }),
            );
        }

        let provider = MockProvider {
            events: tool_call_events(),
        };
        let agent_ctx = Arc::new(std::sync::Mutex::new(AgentContext::default()));
        let emitter = make_emitter(hooks);

        let result = run_stream_session(
            &provider,
            ChatRequest::default(),
            &agent_ctx,
            None,
            &emitter,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.tool_calls.len(), 1);
        let args: serde_json::Value =
            serde_json::from_str(&result.tool_calls[0].arguments).unwrap();
        assert_eq!(args["command"], "log --oneline");
    }
}
