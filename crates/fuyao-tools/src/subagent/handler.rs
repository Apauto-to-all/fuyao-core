//! 子代理工具 handler
//!
//! 流程：
//! 1. upgrade 引擎弱引用 → 拿 [`SubagentOps`]
//! 2. `create_child_session` → `(child_id, rx)`
//! 3. `send(child_id, UserMessage(prompt))`
//! 4. 消费 rx 直到 `Assistant(finish_reason=stop)` → 取 content
//! 5. `end_session(child_id)`（一次性子 session，跑完即退）
//! 6. 返 content 作为工具结果回喂父 ReAct
//!
//! 中间事件（Chunk / ToolCall / ToolResult / 含 tool_calls 的 Assistant）当前丢弃：
//! 子 session 已事件级落库，前端可按 child session_id 查询历史；
//! 后续若需 UI 实时显示子代理进度，可扩展 `ToolCallContext` 加事件转发字段。

use std::time::Duration;

use fuyao_api::message::OutputEvent;
use fuyao_api::message::input::{UserMessage, UserPayload};
use fuyao_api::message::{EventBase, InputEvent};
use fuyao_api::{CancellationToken, ChildSessionSource, SessionParams, ToolCallContext};

/// 子代理执行超时兜底
///
/// 防子 session 卡死（LLM 无响应、工具死循环等）导致父 ReAct 永久阻塞。
/// 触发后强制 end_session 退出。
const SUBAGENT_TIMEOUT: Duration = Duration::from_secs(600);

/// 子代理工具执行入口
///
/// 收 `(args, ctx, cancel)`：args 含 description + prompt，ctx 含引擎弱引用 + 父 session_id。
pub async fn subagent_handler(
    args: serde_json::Value,
    ctx: &ToolCallContext,
    cancel: CancellationToken,
) -> String {
    // 1. 解析参数
    let description = args
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("(无描述)");
    let prompt = match args.get("prompt").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => return "❌ 子代理工具缺少 prompt 参数或为空".to_string(),
    };

    // 2. upgrade SubagentOps（引擎弱引用 → 强引用）
    let Some(ops_weak) = &ctx.subagent_ops else {
        return "❌ 子代理工具未注入引擎能力（SubagentOps 不可用）".to_string();
    };
    let Some(ops) = ops_weak.upgrade() else {
        return "❌ 引擎已关闭，无法派生子代理".to_string();
    };

    // 3. 派生子 session（Fresh 模式：空上下文，子代理不继承父会话历史）
    let parent_id = ctx.session_id.as_deref().unwrap_or("");
    let (child_id, mut child_rx) = match ops
        .create_child_session(
            parent_id,
            ChildSessionSource::Fresh,
            SessionParams::default(),
        )
        .await
    {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!(parent_id, cause = %e, "子代理 session 创建失败");
            return format!("❌ 子代理派生失败：{e}");
        }
    };
    tracing::info!(
        parent_id,
        child_id = %child_id,
        description,
        "子代理 session 已创建"
    );

    // 4. send 任务指令（Guide 模式：立即触发新 turn）
    let msg = InputEvent::User(UserMessage {
        base: EventBase::default(),
        payload: UserPayload {
            content: prompt,
            mode: Default::default(),
            source: Default::default(),
        },
    });
    if let Err(e) = ops.send(&child_id, msg).await {
        tracing::warn!(child_id = %child_id, cause = %e, "子代理任务发送失败");
        let _ = ops.end_session(&child_id, "send 失败").await;
        return format!("❌ 子代理任务发送失败：{e}");
    }

    // 5. 消费子事件流，取 finish_reason=stop 的最终回复
    let mut final_content = String::new();
    let mut got_final = false;
    let deadline = tokio::time::Instant::now() + SUBAGENT_TIMEOUT;

    loop {
        tokio::select! {
            biased; // 取消 / 超时优先

            _ = cancel.cancelled() => {
                tracing::info!(child_id = %child_id, "子代理被父取消");
                let _ = ops.end_session(&child_id, "被父取消").await;
                return "⚠️ 子代理被父取消".to_string();
            }

            _ = tokio::time::sleep_until(deadline) => {
                tracing::warn!(
                    child_id = %child_id,
                    timeout_secs = SUBAGENT_TIMEOUT.as_secs(),
                    "子代理执行超时"
                );
                let _ = ops.end_session(&child_id, "超时").await;
                return format!("❌ 子代理执行超时（{}s）", SUBAGENT_TIMEOUT.as_secs());
            }

            ev = child_rx.recv() => {
                let Some(ev) = ev else {
                    // 子 session task 退出（被外部 end_session 或 panic）
                    tracing::warn!(child_id = %child_id, "子 session 退出，rx 返 None");
                    break;
                };
                // 只关心 finish_reason=stop 的最终 Assistant
                if let OutputEvent::Assistant(m) = ev
                    && m.payload.finish_reason.as_deref() == Some("stop")
                {
                    final_content = m.payload.content.unwrap_or_default();
                    got_final = true;
                    break;
                }
                // 其他事件（Chunk / ToolCall / ToolResult / 含 tool_calls 的 Assistant）丢弃
            }
        }
    }

    // 6. end_session（一次性子 session）
    let _ = ops.end_session(&child_id, "子代理完成").await;
    tracing::info!(child_id = %child_id, got_final, "子代理结束");

    // 7. 返回最终回复
    if !got_final {
        "（子代理 session 退出，未产出最终回复）".to_string()
    } else if final_content.is_empty() {
        "（子代理最终回复为空）".to_string()
    } else {
        final_content
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_error_when_prompt_missing() {
        let ctx = ToolCallContext::default();
        let result = subagent_handler(serde_json::json!({}), &ctx, CancellationToken::new()).await;
        assert!(result.contains("缺少 prompt"), "实际：{result}");
    }

    #[tokio::test]
    async fn returns_error_when_subagent_ops_missing() {
        let ctx = ToolCallContext::default();
        let result = subagent_handler(
            serde_json::json!({"description": "测试", "prompt": "做某事"}),
            &ctx,
            CancellationToken::new(),
        )
        .await;
        assert!(
            result.contains("SubagentOps 不可用"),
            "应有 SubagentOps 缺失错误，实际：{result}"
        );
    }
}
