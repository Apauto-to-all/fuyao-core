//! 子代理工具 handler
//!
//! 流程：
//! 1. upgrade 引擎弱引用 → 拿 [`SubagentOps`]
//! 2. `create_child_session` → `(child_id, rx)`
//! 3. 发 `ChildSession(Started)` 事件通知前端（携带 child_id 供渲染区开辟）
//! 4. `send(child_id, UserMessage(prompt))`
//! 5. 消费 rx：取 `Assistant(finish_reason=stop)` 的 content；中间事件经
//!    `ctx.event_forwarder` 转发到父 session 出站通道（前端实时看子代理进度）
//! 6. `end_session(child_id)`（一次性子 session，跑完即退）
//! 7. 发 `ChildSession(Ended)` 事件通知前端关闭渲染区
//! 8. 返 content 作为工具结果回喂父 ReAct
//!
//! 中间事件透传：子 session 的 Chunk / ToolCall / ToolResult / 含 tool_calls 的 Assistant
//! 经父 session 的 per-session 出站通道进 fan_out（事件 session_id 标的是 child，
//! 前端按 child_id 过滤渲染到子代理区域）。事件已落 DB，转发失败仅 WARN 不阻断。

use super::types::validate_subagent_type;
use std::time::Duration;

use fuyao_api::message::OutputEvent;
use fuyao_api::message::input::{UserMessage, UserPayload};
use fuyao_api::message::output::{
    ChildSessionMessage, ChildSessionOrigin, ChildSessionPayload, ChildSessionState,
};
use fuyao_api::message::{EventBase, InputEvent};
use fuyao_api::{
    AgentConfig, CancellationToken, ChildSessionSource, SessionParams, ToolCallContext,
};

/// 子代理执行超时兜底
///
/// 防子 session 卡死（LLM 无响应、工具死循环等）导致父 ReAct 永久阻塞。
/// 触发后强制 end_session 退出。
const SUBAGENT_TIMEOUT: Duration = Duration::from_secs(600);

/// 子代理工具执行入口
///
/// 收 `(args, ctx, cancel)`：args 含 subagent_type + description + prompt，
/// ctx 含引擎弱引用 + 父 session_id + tool_call_id + event_forwarder（父 session 出站通道的直送克隆）。
/// `subagent_type` 透传为子 session 的 `AgentConfig.definition`（引擎按名加载 `agents/{type}.md`）。
pub async fn subagent_handler(
    args: serde_json::Value,
    ctx: &ToolCallContext,
    cancel: CancellationToken,
) -> String {
    // 1. 解析参数
    let subagent_type = match args.get("subagent_type").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return "❌ 子代理工具缺少 subagent_type 参数或为空".to_string(),
    };
    let description = args
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("(无描述)");
    let prompt = match args.get("prompt").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => return "❌ 子代理工具缺少 prompt 参数或为空".to_string(),
    };

    // 2. 校验 subagent_type：实时查可用列表，不在则返错误 + 列表供 LLM 修正。
    //    生产环境 agent_paths 总是注入；缺失（仅测试场景）时跳过校验不阻断。
    //    此校验杜绝「错误 subagent_type 静默降级为主 Agent 人格」的严重 bug。
    //    置于 SubagentOps upgrade 之前：输入校验优先，失败快返不触碰引擎。
    if let Some(agent_paths) = &ctx.agent_paths {
        if let Err(msg) = validate_subagent_type(&subagent_type, agent_paths) {
            return format!("❌ {msg}");
        }
    } else {
        tracing::warn!("agent_paths 未注入，跳过 subagent_type 校验");
    }

    // 3. upgrade SubagentOps（引擎弱引用 → 强引用）
    let Some(ops_weak) = &ctx.subagent_ops else {
        return "❌ 子代理工具未注入引擎能力（SubagentOps 不可用）".to_string();
    };
    let Some(ops) = ops_weak.upgrade() else {
        return "❌ 引擎已关闭，无法派生子代理".to_string();
    };

    // 4. 派生子 session（Fresh 模式：空上下文，子代理不继承父会话历史）
    //    subagent_type → definition，引擎按名加载 agents/{type}.md（含 mode 校验）
    let parent_id = ctx.session_id.as_deref().unwrap_or("");
    let params = SessionParams {
        agent_config: AgentConfig {
            definition: Some(subagent_type.clone()),
        },
        ..Default::default()
    };
    let (child_id, mut child_rx) = match ops
        .create_child_session(parent_id, ChildSessionSource::Fresh, params)
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
        subagent_type,
        description,
        "子代理 session 已创建"
    );

    // 5. 发 ChildSession(Started)——前端据此开辟子代理渲染区，后续 child session_id 的事件归此区
    emit_child_session_event(ctx, &child_id, ChildSessionState::Started, description);

    // 6. send 任务指令（Guide 模式：立即触发新 turn）
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
        emit_child_session_event(ctx, &child_id, ChildSessionState::Ended, description);
        return format!("❌ 子代理任务发送失败：{e}");
    }

    // 7. 消费子事件流：取 finish_reason=stop 的最终回复；中间事件经 event_forwarder 转发
    let mut final_content = String::new();
    let mut got_final = false;
    let deadline = tokio::time::Instant::now() + SUBAGENT_TIMEOUT;

    loop {
        tokio::select! {
            biased; // 取消 / 超时优先

            _ = cancel.cancelled() => {
                tracing::info!(child_id = %child_id, "子代理被父取消");
                let _ = ops.end_session(&child_id, "被父取消").await;
                emit_child_session_event(ctx, &child_id, ChildSessionState::Ended, description);
                return "⚠️ 子代理被父取消".to_string();
            }

            _ = tokio::time::sleep_until(deadline) => {
                tracing::warn!(
                    child_id = %child_id,
                    timeout_secs = SUBAGENT_TIMEOUT.as_secs(),
                    "子代理执行超时"
                );
                let _ = ops.end_session(&child_id, "超时").await;
                emit_child_session_event(ctx, &child_id, ChildSessionState::Ended, description);
                return format!("❌ 子代理执行超时（{}s）", SUBAGENT_TIMEOUT.as_secs());
            }

            ev = child_rx.recv() => {
                let Some(ev) = ev else {
                    // 子 session task 退出（被外部 end_session 或 panic）
                    tracing::warn!(child_id = %child_id, "子 session 退出，rx 返 None");
                    break;
                };
                // 只关心 finish_reason=stop 的最终 Assistant——它是工具返回值
                if let OutputEvent::Assistant(m) = &ev
                    && m.payload.finish_reason.as_deref() == Some("stop")
                {
                    final_content = m.payload.content.clone().unwrap_or_default();
                    got_final = true;
                    break;
                }
                // 其他事件（Chunk / ToolCall / ToolResult / 含 tool_calls 的 Assistant）：
                // 经父 session 出站通道转发到 fan_out，前端按 child_id 实时渲染子代理进度。
                // 事件 session_id 已是 child，不能被父 emitter 覆盖，所以直送 raw sender。
                if let Some(tx) = &ctx.event_forwarder
                    && tx.send(ev).is_err()
                {
                    tracing::warn!(
                        child_id = %child_id,
                        "event_forwarder 已关闭，子代理中间事件丢弃"
                    );
                    // 不 break——继续消费 rx，最终回复可能仍在路上
                }
                // 无 event_forwarder：静默丢弃（DB 已落库，前端可按 child_id 查历史）
            }
        }
    }

    // 8. end_session（一次性子 session）
    let _ = ops.end_session(&child_id, "子代理完成").await;
    emit_child_session_event(ctx, &child_id, ChildSessionState::Ended, description);
    tracing::info!(child_id = %child_id, got_final, "子代理结束");

    // 9. 返回最终回复
    if !got_final {
        "（子代理 session 退出，未产出最终回复）".to_string()
    } else if final_content.is_empty() {
        "（子代理最终回复为空）".to_string()
    } else {
        final_content
    }
}

/// 发 ChildSession 生命周期事件到父 session 出站通道
///
/// 直接 `tx.send` 绕过 emitter.emit——base.session_id 手动盖父 session 标签
/// （事件由父上下文发出，但描述的是 child 的生命周期）。
fn emit_child_session_event(
    ctx: &ToolCallContext,
    child_session_id: &str,
    state: ChildSessionState,
    description: &str,
) {
    let Some(tx) = &ctx.event_forwarder else {
        return; // 无 forwarder（如未注入）：不发，前端用 ToolResult 兜底
    };
    let parent_session_id = ctx.session_id.clone().unwrap_or_default();
    let state_for_log = state.clone();
    let ev = OutputEvent::ChildSession(ChildSessionMessage {
        base: EventBase {
            session_id: Some(parent_session_id.clone()),
            ..Default::default()
        },
        payload: ChildSessionPayload {
            parent_session_id,
            child_session_id: child_session_id.to_string(),
            origin: ChildSessionOrigin::Subagent,
            state,
            tool_call_id: ctx.tool_call_id.clone(),
            description: description.to_string(),
        },
    });
    if tx.send(ev).is_err() {
        tracing::warn!(
            child_session_id,
            ?state_for_log,
            "event_forwarder 已关闭，ChildSession 事件丢弃"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_error_when_subagent_type_missing() {
        let ctx = ToolCallContext::default();
        let result = subagent_handler(
            serde_json::json!({"description": "测试", "prompt": "做某事"}),
            &ctx,
            CancellationToken::new(),
        )
        .await;
        assert!(result.contains("缺少 subagent_type"), "实际：{result}");
    }

    #[tokio::test]
    async fn returns_error_when_prompt_missing() {
        let ctx = ToolCallContext::default();
        let result = subagent_handler(
            serde_json::json!({"subagent_type": "researcher", "description": "测试"}),
            &ctx,
            CancellationToken::new(),
        )
        .await;
        assert!(result.contains("缺少 prompt"), "实际：{result}");
    }

    #[tokio::test]
    async fn returns_error_when_subagent_ops_missing() {
        let ctx = ToolCallContext::default();
        let result = subagent_handler(
            serde_json::json!({
                "subagent_type": "researcher",
                "description": "测试",
                "prompt": "做某事"
            }),
            &ctx,
            CancellationToken::new(),
        )
        .await;
        assert!(
            result.contains("SubagentOps 不可用"),
            "应有 SubagentOps 缺失错误，实际：{result}"
        );
    }

    #[tokio::test]
    async fn returns_error_when_subagent_type_invalid() {
        // subagent_type 不在可用列表（默认仅内置 researcher/executor）
        // → 校验失败，返错误 + 可用列表，不进入 SubagentOps 路径
        let mut ctx = ToolCallContext::default();
        ctx.agent_paths = Some(fuyao_api::AgentPaths::default());
        let result = subagent_handler(
            serde_json::json!({
                "subagent_type": "nonexistent_type",
                "description": "测试",
                "prompt": "做某事"
            }),
            &ctx,
            CancellationToken::new(),
        )
        .await;
        assert!(
            result.contains("未找到子代理类型"),
            "应拒绝未知 subagent_type，实际：{result}"
        );
        assert!(
            result.contains("researcher") && result.contains("executor"),
            "错误信息应含可用列表，实际：{result}"
        );
    }
}
