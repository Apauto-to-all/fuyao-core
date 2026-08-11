//! 中断机制
//!
//! 三段 select! 中断点（由 [`crate::react`] 驱动）：
//! - 流式期间（LLM 正在吐字）
//! - 工具执行期间（工具 handler 正在跑）
//! - 空队列时（idle，等待新消息）
//!
//! 本模块负责：
//! - [`TurnState`]：单轮共享状态，stream 写、中断分支读（部分结果）。
//! - [`classify`]：集中判断中断场景（避开归档 phase 散落在循环三处的债）。
//! - [`handle_interrupt`]：经 emit_to_history 把补发的 AssistantMessage + 中断式
//!   ToolResult 落进 DB（拦截 → insert_message → 发送）。
//!
//! 锁安全：用 `std::sync::Mutex` + block scope 包裹，**不跨 await 持锁**。
//! 中断分支先 clone 出所需数据再释放锁，然后才 await 发事件。
//!
//! 中断事件 vs 增量结果分离：`OutputEvent::Interrupt`（用户可见通知）由调用方
//! 在 select! 命中时就发；本模块只补增量结果（部分内容、中断式工具结果）。

use crate::dispatch;
use crate::emit::Emitter;
use fuyao_api::InterruptSource;
use fuyao_api::Message;
use fuyao_api::message::output::{
    AssistantMessage, AssistantPayload, InterruptMessage as OutputInterruptMessage,
    InterruptPayload as OutputInterruptPayload, ToolResultMessage, ToolResultPayload,
    build_nested_tool_call,
};
use fuyao_api::message::{EventBase, OutputEvent};
use fuyao_hooks::SharedHooks;
use fuyao_provider::ToolCallData;
use fuyao_session::SessionStore;
use std::sync::{Arc, Mutex};

/// 单轮共享状态
///
/// stream 每收到一个事件就更新这里（累积 text/reasoning/tool_calls）；
/// 中断分支读这里拿部分结果。用 `Arc<Mutex<...>>` 共享，block scope 锁不跨 await。
///
/// 用量统计（usage）不属于这里：它只由模型在流正常完成时给出，
/// 随最终助手消息发出，不进共享状态。
pub(crate) struct TurnState {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCallData>,
}

impl TurnState {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
        }
    }
}

impl Default for TurnState {
    fn default() -> Self {
        Self::new()
    }
}

/// 共享状态的类型别名
pub(crate) type SharedTurnState = Arc<Mutex<TurnState>>;

/// 锁并恢复 poison（与归档一致的 idiom）
fn lock(state: &SharedTurnState) -> std::sync::MutexGuard<'_, TurnState> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

/// 中断场景分类（集中判断，避开归档散落在循环三处的债）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InterruptKind {
    /// 流式期间被中断，且已有工具调用累积
    StreamingWithToolCalls,
    /// 流式期间被中断，无工具调用（纯文本/推理中断）
    Streaming,
}

/// 根据 TurnState 判断中断场景
///
/// 有工具调用累积 → StreamingWithToolCalls；否则 → Streaming。
/// 调用方据此选择发什么增量结果。
pub(crate) fn classify(state: &TurnState) -> InterruptKind {
    if state.tool_calls.is_empty() {
        InterruptKind::Streaming
    } else {
        InterruptKind::StreamingWithToolCalls
    }
}

/// 处理中断：经 emit_to_history 把补发消息落进 DB
///
/// 中断通知事件（`OutputEvent::Interrupt`）已由调用方在 select! 命中时发出，
/// 此处只补增量结果（拦截 → 单条落 DB → 发送事件 → 观察）：
/// - `StreamingWithToolCalls`：补发部分 AssistantMessage（含累积的 tool_calls，
///   finish_reason=interrupted）+ 为每个 tool_call 补发中断式 ToolResult。
/// - `Streaming`：补发部分 AssistantMessage（finish_reason=interrupted，含累积的文本）。
///
/// 补发的消息落 DB 后，下轮 build_chat_request 会从 DB 自然看到
/// 「assistant 调了工具 → 工具结果（中断式）」的完整上下文。
pub(crate) async fn handle_interrupt(
    state: &SharedTurnState,
    kind: InterruptKind,
    interrupt: &OutputInterruptPayload,
    emitter: &Emitter,
    hooks: &SharedHooks,
    store: &SessionStore,
) {
    // 先 clone 出所需数据再释放锁（不跨 await 持锁）
    let (text, reasoning, tool_calls) = {
        let s = lock(state);
        (s.text.clone(), s.reasoning.clone(), s.tool_calls.clone())
    };

    match kind {
        InterruptKind::StreamingWithToolCalls => {
            // 有工具调用累积：补发含 tool_calls 的 AssistantMessage + 中断式 ToolResult
            let valid_tool_calls: Vec<&ToolCallData> = tool_calls
                .iter()
                .filter(|tc| !tc.id.is_empty() && !tc.name.is_empty())
                .collect();

            // 1. 补发中断 AssistantMessage（含累积的 tool_calls）→ 落 DB
            let event = OutputEvent::Assistant(AssistantMessage {
                base: EventBase::default(),
                payload: AssistantPayload {
                    content: if text.is_empty() {
                        None
                    } else {
                        Some(text.clone())
                    },
                    reasoning: if reasoning.is_empty() {
                        None
                    } else {
                        Some(reasoning.clone())
                    },
                    tool_calls: Some(
                        valid_tool_calls
                            .iter()
                            .map(|tc| fuyao_api::message::output::ToolCallPayload {
                                tool_call_id: tc.id.clone(),
                                tool_name: tc.name.clone(),
                                tool_args: serde_json::from_str(&tc.arguments)
                                    .unwrap_or(serde_json::Value::Null),
                            })
                            .collect(),
                    ),
                    finish_reason: Some("interrupted".to_string()),
                    // 中断时模型未给出用量，token 统一记 0
                    completion_tokens: 0,
                    prompt_tokens: 0,
                    total_tokens: 0,
                    reasoning_tokens: 0,
                    cached_tokens: 0,
                },
            });
            // 闭包借用 valid_tool_calls：tool_calls 字段以累积的为准（与事件 payload 一致）
            let _ = dispatch::emit_to_history(emitter, hooks, store, event, |ev| {
                build_interrupted_assistant_msg(ev, &valid_tool_calls)
            })
            .await;

            // 2. 为每个有效 tool_call 补发中断式 ToolResult → 落 DB
            for tc in valid_tool_calls {
                let event = make_interrupt_tool_result(
                    tc.id.clone(),
                    tc.name.clone(),
                    &interrupt.source,
                    &interrupt.reason,
                );
                let _ = dispatch::emit_to_history(emitter, hooks, store, event, |ev| {
                    build_tool_result_msg(ev)
                })
                .await;
            }
        }
        InterruptKind::Streaming => {
            // 纯文本/推理中断：补发部分 AssistantMessage
            if !text.is_empty() || !reasoning.is_empty() {
                let event = OutputEvent::Assistant(AssistantMessage {
                    base: EventBase::default(),
                    payload: AssistantPayload {
                        content: if text.is_empty() {
                            None
                        } else {
                            Some(text.clone())
                        },
                        reasoning: if reasoning.is_empty() {
                            None
                        } else {
                            Some(reasoning.clone())
                        },
                        tool_calls: None,
                        finish_reason: Some("interrupted".to_string()),
                        // 中断时模型未给出用量，token 统一记 0
                        completion_tokens: 0,
                        prompt_tokens: 0,
                        total_tokens: 0,
                        reasoning_tokens: 0,
                        cached_tokens: 0,
                    },
                });
                let _ = dispatch::emit_to_history(emitter, hooks, store, event, |ev| match ev {
                    OutputEvent::Assistant(m) => {
                        let mut msg = Message::assistant(m.payload.content.clone());
                        msg.reasoning = m.payload.reasoning.clone();
                        msg.finish_reason = Some("interrupted".to_string());
                        Some(msg)
                    }
                    _ => None,
                })
                .await;
            }
        }
    }
}

/// 从拦截后的 AssistantMessage 事件构造 interrupted Message（tool_calls 字段以累积的为准）
fn build_interrupted_assistant_msg(
    ev: &OutputEvent,
    valid_tool_calls: &[&ToolCallData],
) -> Option<Message> {
    match ev {
        OutputEvent::Assistant(m) => {
            // schema 构造集中到 build_nested_tool_call，此处不再硬编码字段名
            let tool_calls_json: Vec<serde_json::Value> = valid_tool_calls
                .iter()
                .map(|tc| build_nested_tool_call(&tc.id, &tc.name, &tc.arguments))
                .collect();
            let mut msg = Message::assistant(m.payload.content.clone());
            msg.reasoning = m.payload.reasoning.clone();
            if !tool_calls_json.is_empty() {
                msg.tool_calls = Some(serde_json::Value::Array(tool_calls_json));
            }
            msg.finish_reason = Some("interrupted".to_string());
            Some(msg)
        }
        _ => None,
    }
}

/// 从拦截后的 ToolResult 事件构造 Message::tool_result
fn build_tool_result_msg(ev: &OutputEvent) -> Option<Message> {
    match ev {
        OutputEvent::ToolResult(m) => Some(Message::tool_result(
            m.payload.tool_call_id.clone(),
            m.payload.content.clone(),
        )),
        _ => None,
    }
}

/// 构建中断式 ToolResult 事件
///
/// 中断时工具未真正执行，用合成内容标记中断来源与原因，
/// 让 LLM 下一轮能看到"这个工具调用被中断了"。
fn make_interrupt_tool_result(
    tool_call_id: String,
    tool_name: String,
    source: &InterruptSource,
    reason: &str,
) -> OutputEvent {
    OutputEvent::ToolResult(ToolResultMessage {
        base: EventBase::default(),
        payload: ToolResultPayload {
            tool_call_id,
            tool_name,
            content: format!("[{source:?}][{reason}]"),
        },
    })
}

/// 发送中断通知事件（用户可见的停止信号）
///
/// 入参已是 output 侧 InterruptPayload（内核链路只认 output 侧），
/// 包成 OutputEvent::Interrupt 过 dispatch 管道发出。由 react 在 select! 命中
/// 中断命令时调用。
pub(crate) async fn emit_interrupt_event(
    interrupt: &OutputInterruptPayload,
    emitter: &Emitter,
    hooks: &SharedHooks,
) {
    let event = OutputEvent::Interrupt(OutputInterruptMessage {
        base: EventBase::default(),
        payload: interrupt.clone(),
    });
    dispatch::dispatch(emitter, hooks, event).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_streaming_with_tool_calls() {
        let mut state = TurnState::new();
        state.tool_calls.push(ToolCallData {
            id: "call_1".into(),
            name: "read".into(),
            arguments: "{}".into(),
        });
        assert_eq!(classify(&state), InterruptKind::StreamingWithToolCalls);
    }

    #[test]
    fn classify_streaming_plain() {
        let state = TurnState::new();
        assert_eq!(classify(&state), InterruptKind::Streaming);
    }

    /// 验证 Streaming 中断补发的 AssistantMessage 经 emit_to_history 落进 DB
    ///
    /// 消息已不在内存（事件级落库），通过 load_visible_messages 验证。
    #[tokio::test]
    async fn handle_interrupt_streaming_emits_partial_assistant() {
        let state = Arc::new(Mutex::new(TurnState::new()));
        {
            let mut s = state.lock().unwrap();
            s.text = "部分回复".into();
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let emitter = Emitter::new(tx, "sess1".to_string());
        let hooks: SharedHooks = Arc::new(tokio::sync::Mutex::new(
            fuyao_hooks::HooksRegistry::default(),
        ));
        let interrupt = OutputInterruptPayload::new("用户取消", InterruptSource::User);

        // 构造临时 store + session（消息进 DB）
        let dir =
            std::env::temp_dir().join(format!("fuyao_interrupt_test_{}", uuid::Uuid::new_v4()));
        let store = fuyao_session::SessionStore::new(dir.join("test.db"))
            .await
            .expect("构造 SessionStore 失败");
        let mut session = fuyao_api::Session::new(None, None, None);
        session.id = "sess1".to_string();
        store.create(&session).await.unwrap();
        drop(session);

        handle_interrupt(
            &state,
            InterruptKind::Streaming,
            &interrupt,
            &emitter,
            &hooks,
            &store,
        )
        .await;
        let ev = rx.recv().await.expect("应有事件");
        match ev {
            OutputEvent::Assistant(m) => {
                assert_eq!(m.payload.content.as_deref(), Some("部分回复"));
                assert_eq!(m.payload.finish_reason.as_deref(), Some("interrupted"));
            }
            _ => panic!("应为 Assistant 事件"),
        }
        // 补发的 assistant 消息应进 DB
        let visible = store
            .load_visible_messages("sess1", usize::MAX)
            .await
            .unwrap();
        assert_eq!(visible.len(), 1, "中断补发应落 DB");
        assert_eq!(visible[0].content.as_deref(), Some("部分回复"));
        assert_eq!(visible[0].finish_reason.as_deref(), Some("interrupted"));
    }
}
