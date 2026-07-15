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
//! - [`handle_interrupt`]：发增量结果事件（部分 AssistantMessage / 中断式 ToolResult）。
//!
//! 锁安全：用 `std::sync::Mutex` + block scope 包裹，**不跨 await 持锁**。
//! 中断分支先 clone 出所需数据再释放锁，然后才 await 发事件。
//!
//! 中断事件 vs 增量结果分离：`OutputEvent::Interrupt`（用户可见通知）由调用方
//! 在 select! 命中时就发；本模块只补增量结果（部分内容、中断式工具结果）。

use crate::dispatch;
use crate::emit::Emitter;
use fuyao_api::message::input::{InterruptPayload, InterruptSource};
use fuyao_api::message::output::{
    AssistantMessage, AssistantPayload, InterruptMessage as OutputInterruptMessage,
    InterruptPayload as OutputInterruptPayload, ToolResultMessage, ToolResultPayload,
};
use fuyao_api::message::{EventBase, OutputEvent};
use fuyao_hooks::SharedHooks;
use fuyao_provider::ToolCallData;
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

/// 处理中断：发增量结果事件
///
/// 中断通知事件（`OutputEvent::Interrupt`）已由调用方在 select! 命中时发出，
/// 此处只补增量结果：
/// - `StreamingWithToolCalls`：发部分 AssistantMessage（含已累积的 tool_calls，
///   finish_reason=interrupted）+ 为每个 tool_call 发中断式 ToolResult。
/// - `Streaming`：发部分 AssistantMessage（finish_reason=interrupted，含已累积的文本）。
pub(crate) async fn handle_interrupt(
    state: &SharedTurnState,
    kind: InterruptKind,
    interrupt: &InterruptPayload,
    emitter: &Emitter,
    hooks: &SharedHooks,
) {
    // 先 clone 出所需数据再释放锁（不跨 await 持锁）
    let (text, reasoning, tool_calls) = {
        let s = lock(state);
        (s.text.clone(), s.reasoning.clone(), s.tool_calls.clone())
    };

    match kind {
        InterruptKind::StreamingWithToolCalls => {
            // 有工具调用累积：发含 tool_calls 的 AssistantMessage + 中断式 ToolResult
            let tool_call_payloads: Vec<_> = tool_calls
                .iter()
                .filter(|tc| !tc.id.is_empty() && !tc.name.is_empty())
                .map(|tc| fuyao_api::message::output::ToolCallPayload {
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    tool_args: serde_json::from_str(&tc.arguments)
                        .unwrap_or(serde_json::Value::Null),
                })
                .collect();

            dispatch::dispatch(
                emitter,
                hooks,
                OutputEvent::Assistant(AssistantMessage {
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
                        tool_calls: Some(tool_call_payloads.clone()),
                        finish_reason: Some("interrupted".to_string()),
                        // 中断时模型未给出用量，token 统一记 0
                        completion_tokens: 0,
                        prompt_tokens: 0,
                        total_tokens: 0,
                        reasoning_tokens: 0,
                        cached_tokens: 0,
                    },
                }),
                None,
            )
            .await;

            // 为每个有效 tool_call 发中断式 ToolResult
            for payload in &tool_call_payloads {
                let result = make_interrupt_tool_result(
                    payload.tool_call_id.clone(),
                    payload.tool_name.clone(),
                    &interrupt.source,
                    &interrupt.reason,
                );
                dispatch::dispatch(emitter, hooks, result, None).await;
            }
        }
        InterruptKind::Streaming => {
            // 纯文本/推理中断：发部分 AssistantMessage
            if !text.is_empty() || !reasoning.is_empty() {
                dispatch::dispatch(
                    emitter,
                    hooks,
                    OutputEvent::Assistant(AssistantMessage {
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
                    }),
                    None,
                )
                .await;
            }
        }
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
/// 由 react 在 select! 命中中断命令时调用，转 input Interrupt 为 output Interrupt 事件。
pub(crate) async fn emit_interrupt_event(
    interrupt: &InterruptPayload,
    emitter: &Emitter,
    hooks: &SharedHooks,
) {
    dispatch::dispatch(
        emitter,
        hooks,
        OutputEvent::Interrupt(OutputInterruptMessage {
            base: EventBase::default(),
            payload: OutputInterruptPayload {
                reason: interrupt.reason.clone(),
                source: interrupt.source.clone(),
            },
        }),
        None,
    )
    .await;
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

    #[tokio::test]
    async fn handle_interrupt_streaming_emits_partial_assistant() {
        let state = Arc::new(Mutex::new(TurnState::new()));
        {
            let mut s = state.lock().unwrap();
            s.text = "部分回复".into();
        }
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let emitter = Emitter::new(tx, "sess1".to_string());
        let hooks: SharedHooks = Arc::new(tokio::sync::Mutex::new(
            fuyao_hooks::HooksRegistry::default(),
        ));
        let interrupt = InterruptPayload {
            reason: "用户取消".into(),
            source: InterruptSource::User,
        };
        handle_interrupt(
            &state,
            InterruptKind::Streaming,
            &interrupt,
            &emitter,
            &hooks,
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
    }
}
