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
use fuyao_api::message::input::{PluginEventSource, PluginOrigin};
use fuyao_api::message::output::{
    ErrorMessage, ErrorPayload, PluginMessage, PluginPayload, ToolCallMessage, ToolCallPayload,
};
use fuyao_api::message::{EventBase, OutputEvent};
use fuyao_hooks::LlmErrorAction;
use fuyao_provider::retry::{backoff_duration, is_retryable};
use fuyao_provider::{
    ChatRequest, Provider as LlmProvider, StreamDecoder, StreamError, StreamOptions, StreamUsage,
    ToolCallData as ProviderToolCallData,
};
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

    'stream: loop {
        // 一次锁取全 model_config + agent_paths（AgentContext 是 Arc<Mutex>，禁止重复加锁）
        let (current_model_id, agent_paths, thinking_type, reasoning_effort) = {
            let g = agent_ctx.lock().expect("Agent 上下文锁异常");
            (
                g.model_config
                    .model_id
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                g.agent_paths.clone(),
                g.model_config.thinking_type.clone(),
                g.model_config.reasoning_effort.clone(),
            )
        };
        // API 请求使用短名（"aliyun/qwen3.6-plus" → "qwen3.6-plus"）
        let api_model_name = current_model_id
            .split('/')
            .nth(1)
            .unwrap_or(&current_model_id);

        // 查 Model 元信息，取 reasoning 能力做门控（不支持思考则请求体不发思考字段）
        let model_reasoning = fuyao_provider::get_model(&current_model_id, &agent_paths)
            .map(|m| m.reasoning)
            .unwrap_or(false);

        let options = StreamOptions {
            temperature: None,
            tools: tools_schema.clone(),
            tool_choice: None,
            thinking_type,
            reasoning_effort,
            model_reasoning,
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
                Err(e) if matches!(e, StreamError::ContextOverflow) => {
                    // 上下文溢出：推送插件事件，触发压缩后继续
                    crate::dispatch::dispatch(
                        OutputEvent::Plugin(PluginMessage {
                            base: EventBase::default(),
                            payload: PluginPayload {
                                source: PluginEventSource {
                                    origin: PluginOrigin::Internal,
                                    name: "stream_session".to_string(),
                                },
                                event_type: "context_overflow".to_string(),
                                data: None,
                                error: None,
                                message: Some("上下文溢出，触发压缩...".to_string()),
                            },
                        }),
                        None,
                        emitter,
                    )
                    .await;

                    crate::dispatch::dispatch(
                        OutputEvent::Error(ErrorMessage {
                            base: EventBase::default(),
                            payload: ErrorPayload {
                                message: e.to_string(),
                                recoverable: true,
                            },
                        }),
                        None,
                        emitter,
                    )
                    .await;

                    // 标记退避阶段（中断时区分场景）
                    if let Some(acc) = &accumulator {
                        acc.lock().expect("流式累积器锁异常").phase = StreamPhase::Backoff;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    // 恢复流式阶段
                    if let Some(acc) = &accumulator {
                        acc.lock().expect("流式累积器锁异常").phase = StreamPhase::Streaming;
                    }
                    continue 'stream;
                }
                Err(e) if !is_retryable(&e) => {
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
                            tokio::time::sleep(backoff_duration(retry_count, &e)).await;
                            // 恢复流式阶段
                            if let Some(acc) = &accumulator {
                                acc.lock().expect("流式累积器锁异常").phase =
                                    StreamPhase::Streaming;
                            }
                            continue 'stream;
                        }
                        LlmErrorAction::Abort => {
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

    // 流结束后，从 decoder 取出有效的工具调用并推送 ToolCall 事件
    // TODO: ToolCall 事件与 AssistantMessage 中的 tool_calls 存在部分冲突：
    // output_intercept 修改 ToolCall 事件后，AssistantMessage 中的 tool_calls 不会同步更新，
    // 引擎实际执行的工具调用仍使用原始数据。后续需重构解决此冲突。
    let tool_calls = decoder.take_tool_calls();
    for tc in &tool_calls {
        let args: serde_json::Value =
            serde_json::from_str(&tc.arguments).unwrap_or(serde_json::Value::Null);
        let event = OutputEvent::ToolCall(ToolCallMessage {
            base: EventBase::default(),
            payload: ToolCallPayload {
                tool_call_id: tc.id.clone(),
                tool_name: tc.name.clone(),
                tool_args: args,
            },
        });

        // 统一事件发送
        crate::dispatch::dispatch(event, None, emitter).await;
    }

    Ok(StreamResult {
        text: accumulated_text,
        reasoning: accumulated_reasoning,
        tool_calls,
        usage: decoder.usage().clone(),
    })
}
