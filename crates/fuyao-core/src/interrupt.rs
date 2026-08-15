//! 中断机制
//!
//! 三段 select! 中断点的收尾协议（通知 + 增量补发）唯一归属，由 [`crate::react`]
//! 在命中信号时触发：
//! - 流式期间（LLM 正在吐字）：[`finish_streaming`]
//! - 工具执行期间（工具 handler 正在跑）：[`finish_tool_batch`]
//! - 空队列时（idle，等待新消息）：[`notify_idle`]
//!
//! 三个入口都先发中断通知事件（`OutputEvent::Interrupt`，用户可见的停止信号），
//! 再按阶段补增量结果。shutdown 与用户中断共用收尾协议——[`shutdown_payload`]
//! 提供 source=Shutdown 的载荷，唯一差异是来源标识。
//!
//! 未完成判定的真相源统一为**调用方直接观测的内存状态**，不查 DB：
//! - 流式段：[`TurnState`] 的累积（这批 tool_call 尚未落库，DB 无从回答）
//! - 工具执行段：调用方在结果通道上逐条观测出的已答集合（`answered`）。
//!   已答 = 已经过历史入口处理——含插件 Block（插件的责任，引擎不补偿）与
//!   落库失败（沿用「落库失败已丢弃」降级）
//!
//! 本模块同时持有：
//! - [`TurnState`]：单轮共享状态，stream 写、中断收尾读（部分结果）。
//! - [`classify`]：集中判断中断场景（有无工具调用累积）。
//!
//! 锁安全：用 `std::sync::Mutex` + block scope 包裹，**不跨 await 持锁**。
//! 中断分支先 clone 出所需数据再释放锁，然后才 await 发事件。
//!
//! 中断补发走不计费路径：token 全 0（模型中断时未给用量），不填 model_id。

use crate::dispatch;
use crate::emit::Emitter;
use crate::react::SessionCtx;
use fuyao_api::InterruptSource;
use fuyao_api::ToolCallData;
use fuyao_api::message::output::{
    AssistantMessage, AssistantPayload, InterruptMessage as OutputInterruptMessage,
    InterruptPayload as OutputInterruptPayload, ToolResultMessage, ToolResultPayload,
};
use fuyao_api::message::{EventBase, OutputEvent};
use fuyao_hooks::SharedHooks;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// 单轮共享状态
///
/// stream 每收到一个事件就更新这里（累积 text/reasoning/tool_calls）；
/// 中断收尾读这里拿部分结果。用 `Arc<Mutex<...>>` 共享，block scope 锁不跨 await。
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

/// 中断场景分类（集中判断）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterruptKind {
    /// 流式期间被中断，且已有工具调用累积
    StreamingWithToolCalls,
    /// 流式期间被中断，无工具调用（纯文本/推理中断）
    Streaming,
}

/// 根据累积的工具调用判断中断场景
///
/// 有工具调用累积 → StreamingWithToolCalls；否则 → Streaming。
/// 收尾流程据此决定补发什么增量结果。
fn classify(tool_calls: &[ToolCallData]) -> InterruptKind {
    if tool_calls.is_empty() {
        InterruptKind::Streaming
    } else {
        InterruptKind::StreamingWithToolCalls
    }
}

/// 流式段中断收尾：通知 + 部分结果落库
///
/// select! 中断点①（流式期间，含重试 sleep 期间）命中 shutdown 或 interrupt 时调用。
/// 补发内容按场景：
/// - `StreamingWithToolCalls`：部分 AssistantMessage（含累积的 tool_calls，
///   finish_reason=interrupted）+ 为每个有效 tool_call 补发中断式 ToolResult。
///   工具尚未开始执行（流被截断），全部累积项均未完成。
/// - `Streaming`：部分 AssistantMessage（含累积的文本/推理）。
///
/// 补发的消息落 DB 后，下轮 build_chat_request 会从 DB 自然看到
/// 「assistant 调了工具 → 工具结果（中断式）」的完整上下文。
///
/// 投影 / 落库细节（含 tool_calls 嵌套构造、tool_name 落库）由 history 模块内化，
/// 本函数只负责「从 TurnState 取部分结果、构造事件」。
pub(crate) async fn finish_streaming(
    ctx: &SessionCtx,
    state: &SharedTurnState,
    payload: &OutputInterruptPayload,
) {
    emit_interrupt_event(payload, &ctx.emitter, &ctx.hooks).await;

    // 先 clone 出所需数据再释放锁（不跨 await 持锁）
    let (text, reasoning, tool_calls) = {
        let s = lock(state);
        (s.text.clone(), s.reasoning.clone(), s.tool_calls.clone())
    };

    match classify(&tool_calls) {
        InterruptKind::StreamingWithToolCalls => {
            // 有效工具调用（id 与 name 均非空）才参与补发
            let valid: Vec<&ToolCallData> = tool_calls
                .iter()
                .filter(|tc| is_valid_tool_call(tc))
                .collect();

            // 1. 补发中断 AssistantMessage（含累积的 tool_calls）→ 落 DB
            let event = OutputEvent::Assistant(AssistantMessage {
                base: EventBase::default(),
                payload: interrupted_assistant_payload(&text, &reasoning, Some(&valid)),
            });
            let _ = crate::history::emit_to_history(ctx, event).await;

            // 2. 为每个有效 tool_call 补发中断式 ToolResult → 落 DB
            for tc in valid {
                let event = make_interrupt_tool_result(
                    tc.id.clone(),
                    tc.name.clone(),
                    &payload.source,
                    &payload.reason,
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

/// 工具执行段中断收尾：通知 + 为未完成 tool_call 补发中断式 ToolResult
///
/// select! 中断点②（工具执行期间）命中 shutdown 或 interrupt 时调用。
/// `requested` 为本批请求的全部 tool_call（拦截后的 effective 集合），
/// `answered` 为调用方在结果通道上已观测到并经历史入口处理的 tool_call_id 集
/// （内存真相源，见模块文档）——差集即未完成，逐个补发中断式 ToolResult。
/// 本批的 AssistantMessage 已在工具执行前落库，此处不再补发。
pub(crate) async fn finish_tool_batch(
    ctx: &SessionCtx,
    requested: &[ToolCallData],
    answered: &HashSet<String>,
    payload: &OutputInterruptPayload,
) {
    emit_interrupt_event(payload, &ctx.emitter, &ctx.hooks).await;

    for tc in unfinished_tool_calls(requested, answered) {
        let event = make_interrupt_tool_result(
            tc.id.clone(),
            tc.name.clone(),
            &payload.source,
            &payload.reason,
        );
        let _ = crate::history::emit_to_history(ctx, event).await;
    }
}

/// idle 段中断：无活跃 turn，只发通知事件（无可补发的增量结果）
pub(crate) async fn notify_idle(ctx: &SessionCtx, payload: &OutputInterruptPayload) {
    emit_interrupt_event(payload, &ctx.emitter, &ctx.hooks).await;
    tracing::debug!(session_id = ctx.emitter.session_id(), "idle 时收到中断信号");
}

/// 构造 shutdown 中断载荷（与用户中断共用收尾协议，来源标识不同）
///
/// source=Shutdown / reason="引擎关闭"：UI 收到标准 Interrupt 事件，
/// DB 记录能区分「引擎关闭中断」vs「用户主动中断」。
/// 内核内部产生的中断载荷本就是 output 侧类型，直接构造。
pub(crate) fn shutdown_payload() -> OutputInterruptPayload {
    OutputInterruptPayload::new("引擎关闭", InterruptSource::Shutdown)
}

/// 未完成判定：请求集中剔除已答与无效项
///
/// 已答 = 调用方在结果通道观测到并经历史入口处理（含插件 Block / 落库失败）。
fn unfinished_tool_calls<'a>(
    requested: &'a [ToolCallData],
    answered: &HashSet<String>,
) -> Vec<&'a ToolCallData> {
    requested
        .iter()
        .filter(|tc| is_valid_tool_call(tc) && !answered.contains(&tc.id))
        .collect()
}

/// 工具调用有效（id 与 name 均非空）才参与补发
fn is_valid_tool_call(tc: &ToolCallData) -> bool {
    !tc.id.is_empty() && !tc.name.is_empty()
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
/// 包成 OutputEvent::Interrupt 过 dispatch 管道发出。三个收尾入口共用。
async fn emit_interrupt_event(
    payload: &OutputInterruptPayload,
    emitter: &Emitter,
    hooks: &SharedHooks,
) {
    let event = OutputEvent::Interrupt(OutputInterruptMessage {
        base: EventBase::default(),
        payload: payload.clone(),
    });
    dispatch::dispatch(emitter, hooks, event).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_streaming_with_tool_calls() {
        let tool_calls = vec![ToolCallData {
            id: "call_1".into(),
            name: "read".into(),
            arguments: "{}".into(),
        }];
        assert_eq!(classify(&tool_calls), InterruptKind::StreamingWithToolCalls);
    }

    #[test]
    fn classify_streaming_plain() {
        assert_eq!(classify(&[]), InterruptKind::Streaming);
    }

    /// 未完成判定：已答被剔除、无效（空 id / 空 name）被剔除、顺序保留
    #[test]
    fn unfinished_tool_calls_subtracts_answered_and_invalid() {
        let requested = vec![
            ToolCallData {
                id: "done".into(),
                name: "echo".into(),
                arguments: "{}".into(),
            },
            ToolCallData {
                id: "blocked".into(),
                name: "echo".into(),
                arguments: "{}".into(),
            },
            ToolCallData {
                id: String::new(),
                name: "echo".into(),
                arguments: "{}".into(),
            },
            ToolCallData {
                id: "no_name".into(),
                name: String::new(),
                arguments: "{}".into(),
            },
        ];
        let answered = HashSet::from(["done".to_string()]);
        let unfinished = unfinished_tool_calls(&requested, &answered);
        assert_eq!(unfinished.len(), 1, "已答与无效项都应被剔除");
        assert_eq!(unfinished[0].id, "blocked");
    }

    /// 全部已答时差集为空（无补发）
    #[test]
    fn unfinished_tool_calls_all_answered_is_empty() {
        let requested = vec![ToolCallData {
            id: "done".into(),
            name: "echo".into(),
            arguments: "{}".into(),
        }];
        let answered = HashSet::from(["done".to_string()]);
        assert!(unfinished_tool_calls(&requested, &answered).is_empty());
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

    // finish_streaming / finish_tool_batch 端到端（TurnState → DB 落库 / 差集补发）
    // 由 react/tests.rs 的 interrupt_during_streaming 与
    // interrupt_during_tool_execution_only_completes_unfinished 集成测试覆盖
    // （断言事件流 + load_visible_messages）。
}
