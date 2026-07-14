//! ReAct 循环 task
//!
//! 每个活跃 session 独立运行一个 task，消费消息队列，驱动 ReAct 循环。
//! 第二步为单轮（无工具调用）：User 消息 → LLM 流式回复 → 落库 → 结束。
//! 第三步加工具时，在此循环里扩展「工具调用 → 工具结果 → 再想」分支。
//!
//! 并发模型：多 session 各自一个 task，tokio 调度，异步并发。
//! 同 session 内单 task 串行（从队列取一条、处理完、再取下一条）。

use crate::emit::emit_event;
use crate::engine::types::QueuedMessage;
use crate::stream::{self, StreamResult};
use fuyao_api::message::OutputEvent;
use fuyao_api::message::output::{
    AssistantMessage, AssistantPayload, UserMessage as OutputUserMessage,
};
use fuyao_api::{Message, MessageParams, Session};
use fuyao_provider::{ChatMessage, ChatRequest, Provider, StreamDecoder, StreamOptions};
use std::sync::Arc;
use tokio::sync::mpsc::{Receiver, Sender};

/// session 的独立执行流
///
/// 消费 `rx`（消息队列），驱动 ReAct 循环。通道关闭（所有 Sender drop）时退出。
pub(crate) async fn run_session(
    session_id: String,
    mut rx: Receiver<QueuedMessage>,
    mut session: Session,
    store: Arc<fuyao_session::SessionStore>,
    provider: Arc<dyn Provider>,
    tx_event: Sender<OutputEvent>,
) {
    tracing::info!(session_id = %session_id, "session 执行流启动");

    while let Some(msg) = rx.recv().await {
        match msg.event {
            fuyao_api::InputEvent::User(user_msg) => {
                handle_user_message(
                    &session_id,
                    &mut session,
                    &store,
                    &provider,
                    &tx_event,
                    user_msg.payload.content,
                    msg.params.unwrap_or_default(),
                )
                .await;
            }
            fuyao_api::InputEvent::Interrupt(_) => {
                // 第二步：中断处理留给第三步（和工具调用、重试退避一起）
                tracing::debug!(session_id = %session_id, "收到中断信号（第二步暂不处理）");
            }
            fuyao_api::InputEvent::Plugin(plugin_msg) => {
                // 插件通知：转发为 OutputEvent::Plugin（input → output payload 重组）
                emit_event(
                    &tx_event,
                    &session_id,
                    OutputEvent::Plugin(fuyao_api::message::output::PluginMessage {
                        base: plugin_msg.base,
                        payload: fuyao_api::message::output::PluginPayload {
                            source: plugin_msg.payload.source,
                            event_type: plugin_msg.payload.event_type,
                            data: plugin_msg.payload.data,
                            error: plugin_msg.payload.error,
                            message: plugin_msg.payload.message,
                        },
                    }),
                )
                .await;
            }
        }
    }

    tracing::info!(session_id = %session_id, "session 执行流结束");
}

/// 处理一条用户消息：入历史 → 调 LLM → 发结果 → 落库
async fn handle_user_message(
    session_id: &str,
    session: &mut Session,
    store: &Arc<fuyao_session::SessionStore>,
    provider: &Arc<dyn Provider>,
    tx_event: &Sender<OutputEvent>,
    content: String,
    params: MessageParams,
) {
    // TODO: 第三步加 pending 队列——工具执行期间用户补充的消息暂存 pending，
    //       等 AI 不再调工具才转入 guide。第二步无工具调用，暂不需要。

    // 1. User 消息进内存历史
    session.messages.push(Message::user(content.clone()));

    // 2. 发 User 事件（回显给 UI）
    emit_event(
        tx_event,
        session_id,
        OutputEvent::User(OutputUserMessage {
            base: fuyao_api::message::EventBase::default(),
            payload: fuyao_api::message::output::UserPayload {
                content: content.clone(),
                mode: fuyao_api::UserMessageMode::Guide,
                source: fuyao_api::UserMessageSource::User,
            },
        }),
    )
    .await;

    // 3. 凑 ChatRequest（系统提示词单独填 system，不进 messages）
    let request = build_chat_request(session);

    // 4. 从 MessageParams 解析模型 + 构建流式选项
    let (model, options) = build_model_and_options(&params);

    // 5. 流式调用 LLM
    let mut decoder = StreamDecoder::new();
    let result = stream::run_stream_session(
        request,
        &model,
        options,
        provider,
        tx_event,
        session_id,
        &mut decoder,
    )
    .await;

    match result {
        Ok(stream_result) => {
            // 6. 组装 Assistant 消息，进内存历史
            let usage = decoder.usage().clone();
            let assistant_msg = build_assistant_message(
                &stream_result,
                &usage,
                params.model_config.model_id.as_deref(),
            );
            session.messages.push(assistant_msg);

            // 7. 发 Assistant 事件
            emit_event(
                tx_event,
                session_id,
                OutputEvent::Assistant(AssistantMessage {
                    base: fuyao_api::message::EventBase::default(),
                    payload: assistant_msg_to_payload(&stream_result, &usage),
                }),
            )
            .await;

            // 8. 落库（边界时刻：User + Assistant 都已进 messages，增量保存）
            persist(session_id, session, store).await;
        }
        Err(_) => {
            // stream_session 已发过 Error 事件，这里只记日志
            tracing::warn!(
                session_id = session_id,
                "LLM 调用失败，本轮未产出 Assistant 消息"
            );
            // User 消息已进内存但 Assistant 未产出——仍落库以便下次恢复能看到
            persist(session_id, session, store).await;
        }
    }
}

/// 落库（边界时刻调用）
async fn persist(
    session_id: &str,
    session: &mut Session,
    store: &Arc<fuyao_session::SessionStore>,
) {
    session.message_count = session.messages.len() as i64;
    if let Err(e) = store.update(session).await {
        tracing::warn!(session_id = session_id, cause = %e, "session 落库失败");
    }
}

/// 从 session 的内存历史凑 ChatRequest
///
/// 系统提示词单独填 request.system（不进 messages 数组），
/// messages 只装 user/assistant/tool 对话历史。
fn build_chat_request(session: &Session) -> ChatRequest {
    let messages = session
        .messages
        .iter()
        .map(|m| ChatMessage {
            role: m.role.clone(),
            content: m.content.clone(),
            reasoning: m.reasoning.clone(),
            tool_calls: m.tool_calls.as_ref().and_then(|tc| tc.as_array().cloned()),
            tool_call_id: m.tool_call_id.clone(),
            tool_name: m.tool_name.clone(),
        })
        .collect();

    ChatRequest {
        messages,
        system: session.system_prompt.clone(),
    }
}

/// 从 MessageParams 解析模型名 + 构建流式选项
///
/// model_id 格式 "provider/model" → 取 '/' 后的 model 部分。
/// 思考控制参数（thinking_type / reasoning_effort）透传给 StreamOptions。
// TODO: model_id 为 None 时用配置的默认模型（第二步先返回空串，provider 自行处理）
fn build_model_and_options(params: &MessageParams) -> (String, StreamOptions) {
    let model = params
        .model_config
        .model_id
        .as_deref()
        .and_then(|id| id.split('/').nth(1))
        .unwrap_or("")
        .to_string();

    let options = StreamOptions {
        temperature: None,
        tools: None, // 第二步无工具
        tool_choice: None,
        thinking_type: params.model_config.thinking_type.clone(),
        reasoning_effort: params.model_config.reasoning_effort.clone(),
    };

    (model, options)
}

/// 从流式结果构建 Assistant Message（用于进内存历史）
fn build_assistant_message(
    result: &StreamResult,
    _usage: &fuyao_provider::StreamUsage,
    model_id: Option<&str>,
) -> Message {
    let mut msg = Message::assistant(if result.text.is_empty() {
        None
    } else {
        Some(result.text.clone())
    });
    if !result.reasoning.is_empty() {
        msg.reasoning = Some(result.reasoning.clone());
    }
    msg.model_id = model_id.map(|s| s.to_string());
    msg.finish_reason = Some("stop".to_string());
    msg
}

/// 从流式结果构建 AssistantPayload（用于发事件）
fn assistant_msg_to_payload(
    result: &StreamResult,
    usage: &fuyao_provider::StreamUsage,
) -> AssistantPayload {
    AssistantPayload {
        content: if result.text.is_empty() {
            None
        } else {
            Some(result.text.clone())
        },
        reasoning: if result.reasoning.is_empty() {
            None
        } else {
            Some(result.reasoning.clone())
        },
        tool_calls: None, // 第二步无工具
        finish_reason: Some("stop".to_string()),
        completion_tokens: usage.completion_tokens as i64,
        prompt_tokens: usage.prompt_tokens as i64,
        total_tokens: usage.total_tokens as i64,
        reasoning_tokens: usage.completion_reasoning_tokens.unwrap_or(0) as i64,
        cached_tokens: usage.prompt_cached_tokens.unwrap_or(0) as i64,
    }
}
