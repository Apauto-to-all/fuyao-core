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
//! - [`handle_interrupt`]：经统一历史入口（[`crate::history`]）把补发的
//!   AssistantMessage + 中断式 ToolResult 落进 DB（拦截 → insert_message → 发送）。
//!
//! 锁安全：用 `std::sync::Mutex` + block scope 包裹，**不跨 await 持锁**。
//! 中断分支先 clone 出所需数据再释放锁，然后才 await 发事件。
//!
//! 中断事件 vs 增量结果分离：`OutputEvent::Interrupt`（用户可见通知）由调用方
//! 在 select! 命中时就发；本模块只补增量结果（部分内容、中断式工具结果）。
//! 中断补发走不计费路径：token 全 0（模型中断时未给用量），不填 model_id。

use crate::dispatch;
use crate::emit::Emitter;
use crate::react::SessionCtx;
use fuyao_api::InterruptSource;
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

/// 处理中断：经统一历史入口把补发消息落进 DB
///
/// 中断通知事件（`OutputEvent::Interrupt`）已由调用方在 select! 命中时发出，
/// 此处只补增量结果（拦截 → 单条落 DB → 发送事件 → 观察）：
/// - `StreamingWithToolCalls`：补发部分 AssistantMessage（含累积的 tool_calls，
///   finish_reason=interrupted）+ 为每个 tool_call 补发中断式 ToolResult。
/// - `Streaming`：补发部分 AssistantMessage（finish_reason=interrupted，含累积的文本）。
///
/// 补发的消息落 DB 后，下轮 build_chat_request 会从 DB 自然看到
/// 「assistant 调了工具 → 工具结果（中断式）」的完整上下文。
///
/// 投影 / 落库细节（含 tool_calls 嵌套构造、tool_name 落库）由 history 模块内化，
/// 本函数只负责「从 TurnState 取部分结果、构造事件」。
pub(crate) async fn handle_interrupt(
    state: &SharedTurnState,
    kind: InterruptKind,
    interrupt: &OutputInterruptPayload,
    ctx: &SessionCtx,
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
                payload: interrupted_assistant_payload(&text, &reasoning, Some(&valid_tool_calls)),
            });
            let _ = crate::history::emit_to_history(ctx, event).await;

            // 2. 为每个有效 tool_call 补发中断式 ToolResult → 落 DB
            for tc in valid_tool_calls {
                let event = make_interrupt_tool_result(
                    tc.id.clone(),
                    tc.name.clone(),
                    &interrupt.source,
                    &interrupt.reason,
                );
                let _ = crate::history::emit_to_history(ctx, event).await;
            }
        }
        InterruptKind::Streaming => {
            // 纯文本/推理中断：补发部分 AssistantMessage
            if !text.is_empty() || !reasoning.is_empty() {
                let event = OutputEvent::Assistant(AssistantMessage {
                    base: EventBase::default(),
                    payload: interrupted_assistant_payload(&text, &reasoning, None),
                });
                let _ = crate::history::emit_to_history(ctx, event).await;
            }
        }
    }
}

/// 构造中断补发的 AssistantPayload（token 全 0 约定的唯一出处）
///
/// 两个分支（有 / 无工具调用累积）共用：content / reasoning 空串归 None，
/// finish_reason=interrupted，token 五字段全 0（模型中断时未给用量）。
fn interrupted_assistant_payload(
    text: &str,
    reasoning: &str,
    tool_calls: Option<&[&ToolCallData]>,
) -> AssistantPayload {
    AssistantPayload {
        content: if text.is_empty() {
            None
        } else {
            Some(text.to_string())
        },
        reasoning: if reasoning.is_empty() {
            None
        } else {
            Some(reasoning.to_string())
        },
        tool_calls: tool_calls.map(|calls| {
            calls
                .iter()
                .map(|tc| fuyao_api::message::output::ToolCallPayload {
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    tool_args: serde_json::from_str(&tc.arguments)
                        .unwrap_or(serde_json::Value::Null),
                })
                .collect()
        }),
        finish_reason: Some("interrupted".to_string()),
        // 中断时模型未给出用量，token 统一记 0
        completion_tokens: 0,
        prompt_tokens: 0,
        total_tokens: 0,
        reasoning_tokens: 0,
        cached_tokens: 0,
    }
}

/// 构建中断式 ToolResult 事件
///
/// 中断时工具未真正执行，用合成内容标记中断来源与原因，
/// 让 LLM 下一轮能看到"这个工具调用被中断了"。
///
/// 中断式 ToolResult 的构造集中在此（turn.rs 在为未完成 tool_call 补发中断结果时复用，
/// 不再各写一份相同的 `format!("[{source:?}][{reason}]")`）。
pub(crate) fn make_interrupt_tool_result(
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

    /// 中断 payload 的 token 约定：五字段全 0、finish_reason=interrupted
    #[test]
    fn interrupted_assistant_payload_zero_tokens() {
        let p = interrupted_assistant_payload("部分回复", "思考", None);
        assert_eq!(p.content.as_deref(), Some("部分回复"));
        assert_eq!(p.reasoning.as_deref(), Some("思考"));
        assert_eq!(p.finish_reason.as_deref(), Some("interrupted"));
        assert_eq!(p.prompt_tokens, 0);
        assert_eq!(p.completion_tokens, 0);
        assert!(p.tool_calls.is_none());
    }

    /// 空文本 + 空推理时 content / reasoning 归 None（不落空串）
    #[test]
    fn interrupted_assistant_payload_empty_text_is_none() {
        let p = interrupted_assistant_payload("", "", None);
        assert_eq!(p.content, None);
        assert_eq!(p.reasoning, None);
    }

    /// 有工具调用累积时 payload 携带扁平 tool_calls（arguments 解析失败归 Null）
    #[test]
    fn interrupted_assistant_payload_carries_tool_calls() {
        let tc = ToolCallData {
            id: "call_1".into(),
            name: "search".into(),
            arguments: "{\"q\":\"rust\"}".into(),
        };
        let bad = ToolCallData {
            id: "call_2".into(),
            name: "bad".into(),
            arguments: "not-json".into(),
        };
        let p = interrupted_assistant_payload("", "", Some(&[&tc, &bad]));
        let calls = p.tool_calls.expect("应携带 tool_calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].tool_name, "search");
        assert_eq!(calls[0].tool_args, serde_json::json!({"q": "rust"}));
        assert_eq!(calls[1].tool_args, serde_json::Value::Null);
    }

    // handle_interrupt 端到端（TurnState → DB 落库）由 react/tests.rs 的
    // interrupt_during_streaming 集成测试覆盖（断言事件流 + load_visible_messages）。
}
