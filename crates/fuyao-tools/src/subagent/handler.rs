//! 子代理工具 handler
//!
//! 流程：
//! 1. upgrade 引擎弱引用 → 拿 [`SubagentOps`]
//! 2. `create_child_session` → `(child_id, rx)`
//! 3. 发 `ChildSession(Started)` 事件通知前端（携带 child_id 供渲染区开辟）
//! 4. `send(child_id, UserMessage(prompt))`
//! 5. 消费 rx：全部事件经 `ctx.capabilities.event_forwarder` 转发到父 session
//!    出站通道（前端实时看子代理进度）；`Assistant(finish_reason=stop)` 的
//!    content 截留为工具返回值，事件本身照常转发
//! 6. `destroy_session(child_id)`（一次性子 session，跑完即退）
//! 7. 排空通道滞留事件继续转发（子 task 收尾产物：Title / 中断终态等）
//! 8. 发 `ChildSession(Ended)` 事件通知前端关闭渲染区
//! 9. 返 content 作为工具结果回喂父 ReAct
//!
//! 事件全量透传：子 session 的全部事件（Chunk / ToolCall / ToolResult / Assistant /
//! Title 等，含 finish_reason=stop 的终态）经父 session 的 per-session 出站通道进
//! fan_out（事件 session_id 标的是 child，前端按 child_id 过滤渲染到子代理区域）。
//! 终态 Assistant 必须转发：前端子会话运行状态机只认终态事件（非 tool_calls 的
//! Assistant / Interrupt / Error）回 idle，截留不发会让子会话永久显示运行中。
//! 事件已落 DB，转发失败仅 WARN 不阻断。

use super::types::{SubagentArgs, validate_subagent_type};

use tokio::sync::mpsc;

use crate::common::parse_tool_args;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::input::{UserMessage, UserPayload};
use fuyao_api::message::output::{
    ChildSessionMessage, ChildSessionOrigin, ChildSessionPayload, ChildSessionState,
};
use fuyao_api::message::{EventBase, InputEvent};
use fuyao_api::{AgentConfig, CancellationToken, ChildSessionSource, ToolCallContext, ToolOutput};

/// 子代理工具执行入口
///
/// 收 `(args, ctx, cancel)`：args 含 subagent_type + description + prompt，
/// ctx 含引擎弱引用 + 父 session_id + tool_call_id + event_forwarder（父 session 出站通道的直送克隆）。
/// `subagent_type` 透传为子 session 的 `AgentConfig.definition`（引擎按名加载 `agents/{type}.md`）。
pub async fn subagent_handler(
    args: serde_json::Value,
    ctx: ToolCallContext,
    cancel: CancellationToken,
) -> ToolOutput {
    // 1. 解析参数（类型化：subagent_type / prompt 必填，description 可缺省）
    let SubagentArgs {
        subagent_type,
        description,
        prompt,
    } = match parse_tool_args(args) {
        Ok(a) => a,
        Err(e) => return e,
    };
    if subagent_type.trim().is_empty() {
        return ToolOutput::error("子代理工具的 subagent_type 不能为空");
    }
    if prompt.trim().is_empty() {
        return ToolOutput::error("子代理工具的 prompt 不能为空");
    }
    let description = description.as_deref().unwrap_or("(无描述)");

    // 2. 校验 subagent_type：实时查可用列表，不在则返错误 + 列表供 LLM 修正。
    //    生产环境 agent_paths 总是注入；缺失（仅测试场景）时跳过校验不阻断。
    //    此校验杜绝「错误 subagent_type 静默降级为主 Agent 人格」的严重 bug。
    //    置于 SubagentOps upgrade 之前：输入校验优先，失败快返不触碰引擎。
    if let Some(agent_paths) = &ctx.agent_paths {
        if let Err(msg) = validate_subagent_type(&subagent_type, agent_paths) {
            return ToolOutput::error(msg);
        }
    } else {
        tracing::warn!("agent_paths 未注入，跳过 subagent_type 校验");
    }

    // 3. upgrade SubagentOps（引擎弱引用 → 强引用）
    let Some(ops_weak) = &ctx.capabilities.subagent_ops else {
        return ToolOutput::error("子代理工具未注入引擎能力（SubagentOps 不可用）");
    };
    let Some(ops) = ops_weak.upgrade() else {
        return ToolOutput::error("引擎已关闭，无法派生子代理");
    };

    // 4. 派生子 session（Fresh 模式：空上下文，子代理不继承父会话历史）
    //    subagent_type → definition，引擎按名加载 agents/{type}.md（含 mode 校验）
    let parent_id = ctx.session_id.as_deref().unwrap_or("");
    // 子代理人格配置（definition）；model_config 由引擎从父 session 继承，此处不传
    let child_agent_config = AgentConfig {
        definition: subagent_type.clone(),
    };
    let (child_id, mut child_rx) = match ops
        .create_child_session(parent_id, ChildSessionSource::Fresh, child_agent_config)
        .await
    {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!(parent_id, cause = %e, "子代理 session 创建失败");
            return ToolOutput::error(format!("子代理派生失败：{e}"));
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
    emit_child_session_event(&ctx, &child_id, ChildSessionState::Started, description);

    // 6. send 任务指令（Guide 模式：立即触发新 turn）
    let msg = InputEvent::User(UserMessage {
        base: EventBase::default(),
        payload: UserPayload {
            content: prompt,
            images: vec![],
            mode: Default::default(),
            source: Default::default(),
            client_message_id: None,
        },
    });
    if let Err(e) = ops.send(&child_id, msg).await {
        tracing::warn!(child_id = %child_id, cause = %e, "子代理任务发送失败");
        let _ = ops.destroy_session(&child_id, "send 失败").await;
        emit_child_session_event(&ctx, &child_id, ChildSessionState::Ended, description);
        return ToolOutput::error(format!("子代理任务发送失败：{e}"));
    }

    // 7. 消费子事件流：全部事件转发进父 session 出站通道；finish_reason=stop 的
    //    终态 Assistant 同时截留 content 作工具返回值
    let mut final_content = String::new();
    let mut got_final = false;

    loop {
        tokio::select! {
            biased; // 取消优先

            _ = cancel.cancelled() => {
                tracing::info!(child_id = %child_id, "子代理被父取消");
                let _ = ops.destroy_session(&child_id, "被父取消").await;
                drain_child_events(&ctx, &child_id, &mut child_rx);
                emit_child_session_event(&ctx, &child_id, ChildSessionState::Ended, description);
                return ToolOutput::text("⚠️ 子代理被父取消");
            }

            ev = child_rx.recv() => {
                let Some(ev) = ev else {
                    // 子 session task 退出（被外部 destroy_session 或 panic）
                    tracing::warn!(child_id = %child_id, "子 session 退出，rx 返 None");
                    break;
                };
                // 终态 Assistant（finish_reason=stop）：content 截留为工具返回值
                let is_final = match &ev {
                    OutputEvent::Assistant(m)
                        if m.payload.finish_reason.as_deref() == Some("stop") =>
                    {
                        final_content = m.payload.content.clone().unwrap_or_default();
                        got_final = true;
                        true
                    }
                    _ => false,
                };
                // 终态与其余事件（Chunk / ToolCall / ToolResult / 含 tool_calls 的
                // Assistant / Title 等）一律转发——终态 Assistant 是前端子会话运行
                // 状态回 idle 的唯一正常信号
                forward_child_event(&ctx, &child_id, ev);
                if is_final {
                    break;
                }
            }
        }
    }

    // 8. destroy_session（一次性子 session）——task 退出后通道里滞留的收尾产物
    //    （Title 等）非阻塞排空转发，之后丢弃接收端
    let _ = ops.destroy_session(&child_id, "子代理完成").await;
    drain_child_events(&ctx, &child_id, &mut child_rx);
    emit_child_session_event(&ctx, &child_id, ChildSessionState::Ended, description);
    tracing::info!(child_id = %child_id, got_final, "子代理结束");

    // 9. 返回最终回复（纯文本回喂，不裹 JSON）
    if !got_final {
        ToolOutput::text("（子代理 session 退出，未产出最终回复）")
    } else if final_content.is_empty() {
        ToolOutput::text("（子代理最终回复为空）")
    } else {
        ToolOutput::text(final_content)
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
    let Some(tx) = &ctx.capabilities.event_forwarder else {
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

/// 转发一条子事件到父 session 出站通道
///
/// 事件 session_id 已是 child，不能被父 emitter 覆盖，所以直送 raw sender。
/// 无 event_forwarder（未注入）时静默丢弃（事件已落 DB，前端可按 child_id 查
/// 历史）；转发失败（通道关闭）仅 WARN，不阻碍后续事件消费。
fn forward_child_event(ctx: &ToolCallContext, child_id: &str, ev: OutputEvent) {
    if let Some(tx) = &ctx.capabilities.event_forwarder
        && tx.send(ev).is_err()
    {
        tracing::warn!(child_id, "event_forwarder 已关闭，子代理事件丢弃");
    }
}

/// 非阻塞排空子事件通道的滞留事件并逐条转发
///
/// `destroy_session` 返回时子 task 已退出，其收尾产物（中断式终态、Title 等）仍滞留
/// 通道缓冲——逐条转发后再丢弃接收端，保证转发的子事件流有始有终。用 try_recv
/// 非阻塞排空：标题生成等旁路 task 仍持 sender 克隆，通道不会关闭，await 式
/// 排空会挂住。
fn drain_child_events(
    ctx: &ToolCallContext,
    child_id: &str,
    rx: &mut mpsc::UnboundedReceiver<OutputEvent>,
) {
    while let Ok(ev) = rx.try_recv() {
        forward_child_event(ctx, child_id, ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex, Weak};

    use fuyao_api::message::output::{
        AssistantMessage, AssistantPayload, ChunkMessage, ChunkPayload, InterruptMessage,
        InterruptPayload, TitleMessage, TitlePayload,
    };
    use fuyao_api::{InterruptSource, SubagentOps};

    // ── 测试辅助 ─────────────────────────────────────────────

    /// 伪造 SubagentOps：`create_child_session` 交出预灌好事件的 rx，send / end 恒成功
    #[allow(clippy::type_complexity)]
    struct FakeSubagentOps {
        /// 预构造的子事件通道接收端（create_child_session 交出）
        child_rx: Mutex<Option<mpsc::UnboundedReceiver<OutputEvent>>>,
    }

    impl SubagentOps for FakeSubagentOps {
        fn create_child_session<'a>(
            &'a self,
            _parent_session_id: &'a str,
            _source: ChildSessionSource,
            _child_agent_config: AgentConfig,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<(String, mpsc::UnboundedReceiver<OutputEvent>), String>>
                    + Send
                    + 'a,
            >,
        > {
            let rx = self.child_rx.lock().unwrap().take();
            Box::pin(async move {
                rx.map(|rx| ("child-1".to_string(), rx))
                    .ok_or_else(|| "测试 rx 未预置".to_string())
            })
        }

        fn send<'a>(
            &'a self,
            _id: &'a str,
            _event: InputEvent,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }

        fn destroy_session<'a>(
            &'a self,
            _id: &'a str,
            _end_reason: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }
    }

    /// 构造带伪造 SubagentOps 与 event_forwarder 的工具上下文
    ///
    /// 返回的 `Arc` 是弱引用的强句柄持有方（生产环境由引擎承担）——测试须持有到
    /// handler 调用结束，否则 Weak upgrade 失败走「引擎已关闭」分支。
    fn fake_ops_ctx(
        child_rx: mpsc::UnboundedReceiver<OutputEvent>,
        fwd_tx: mpsc::UnboundedSender<OutputEvent>,
    ) -> (Arc<dyn SubagentOps>, ToolCallContext) {
        let ops: Arc<dyn SubagentOps> = Arc::new(FakeSubagentOps {
            child_rx: Mutex::new(Some(child_rx)),
        });
        let weak: Weak<dyn SubagentOps> = Arc::downgrade(&ops);
        (
            ops,
            ToolCallContext {
                session_id: Some("parent-1".into()),
                capabilities: fuyao_api::ToolCapabilities {
                    subagent_ops: Some(weak),
                    event_forwarder: Some(fwd_tx),
                    todo_store: None,
                },
                ..ToolCallContext::default()
            },
        )
    }

    /// 逐条收空转发通道，把事件折叠为种类标签序列
    fn collect_forwarded_kinds(rx: &mut mpsc::UnboundedReceiver<OutputEvent>) -> Vec<&'static str> {
        let mut kinds = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            kinds.push(match ev {
                OutputEvent::Chunk(_) => "chunk",
                OutputEvent::Assistant(_) => "assistant",
                OutputEvent::Title(_) => "title",
                OutputEvent::Interrupt(_) => "interrupt",
                OutputEvent::ChildSession(m) => match m.payload.state {
                    ChildSessionState::Started => "child-started",
                    ChildSessionState::Ended => "child-ended",
                },
                _ => "other",
            });
        }
        kinds
    }

    /// 中间流式片段事件
    fn chunk_event(content: &str) -> OutputEvent {
        OutputEvent::Chunk(ChunkMessage {
            base: EventBase::default(),
            payload: ChunkPayload {
                content: Some(content.to_string()),
                reasoning: None,
            },
        })
    }

    /// 终态助手事件（finish_reason=stop）
    fn final_assistant_event(content: &str) -> OutputEvent {
        OutputEvent::Assistant(AssistantMessage {
            base: EventBase::default(),
            payload: AssistantPayload {
                content: Some(content.to_string()),
                reasoning: None,
                tool_calls: None,
                finish_reason: Some("stop".to_string()),
                completion_tokens: 0,
                prompt_tokens: 0,
                total_tokens: 0,
                reasoning_tokens: 0,
                cached_tokens: 0,
            },
        })
    }

    /// 标题更新事件（fire-and-forget 旁路产物，常滞留在通道缓冲）
    fn title_event(title: &str) -> OutputEvent {
        OutputEvent::Title(TitleMessage {
            base: EventBase::default(),
            payload: TitlePayload {
                title: title.to_string(),
            },
        })
    }

    /// 中断终态事件（子 session 收尾落库前发出）
    fn interrupt_event() -> OutputEvent {
        OutputEvent::Interrupt(InterruptMessage {
            base: EventBase::default(),
            payload: InterruptPayload::new("被父取消", InterruptSource::User),
        })
    }

    /// 合法子代理调用参数
    fn valid_args() -> serde_json::Value {
        serde_json::json!({
            "subagent_type": "explore",
            "description": "测试",
            "prompt": "做某事"
        })
    }

    #[tokio::test]
    async fn returns_error_when_subagent_type_missing() {
        let ctx = ToolCallContext::default();
        let result = subagent_handler(
            serde_json::json!({"description": "测试", "prompt": "做某事"}),
            ctx,
            CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("subagent_type"), "实际：{result}");
    }

    #[tokio::test]
    async fn returns_error_when_prompt_missing() {
        let ctx = ToolCallContext::default();
        let result = subagent_handler(
            serde_json::json!({"subagent_type": "explore", "description": "测试"}),
            ctx,
            CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("prompt"), "实际：{result}");
    }

    #[tokio::test]
    async fn returns_error_when_subagent_ops_missing() {
        let ctx = ToolCallContext::default();
        let result = subagent_handler(
            serde_json::json!({
                "subagent_type": "explore",
                "description": "测试",
                "prompt": "做某事"
            }),
            ctx,
            CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(
            result.contains("SubagentOps 不可用"),
            "应有 SubagentOps 缺失错误，实际：{result}"
        );
    }

    #[tokio::test]
    async fn returns_error_when_subagent_type_invalid() {
        // subagent_type 不在可用列表（默认仅内置 explore/executor）
        // → 校验失败，返错误 + 可用列表，不进入 SubagentOps 路径
        let ctx = ToolCallContext {
            agent_paths: Some(fuyao_api::AgentPaths::default()),
            ..ToolCallContext::default()
        };
        let result = subagent_handler(
            serde_json::json!({
                "subagent_type": "nonexistent_type",
                "description": "测试",
                "prompt": "做某事"
            }),
            ctx,
            CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(
            result.contains("未找到子代理类型"),
            "应拒绝未知 subagent_type，实际：{result}"
        );
        assert!(
            result.contains("explore") && result.contains("executor"),
            "错误信息应含可用列表，实际：{result}"
        );
    }

    #[tokio::test]
    async fn forwards_final_assistant_and_drains_residual_events() {
        // 预灌子事件流：中间 Chunk → 终态 Assistant → 滞留通道的 Title
        // （unbounded 通道缓冲，handler 后续逐条消费）
        let (child_tx, child_rx) = mpsc::unbounded_channel();
        child_tx.send(chunk_event("部分输出")).unwrap();
        child_tx.send(final_assistant_event("探索完成")).unwrap();
        child_tx.send(title_event("探索任务")).unwrap();
        drop(child_tx);

        let (fwd_tx, mut fwd_rx) = mpsc::unbounded_channel();
        let (_ops_guard, ctx) = fake_ops_ctx(child_rx, fwd_tx);

        let out = subagent_handler(valid_args(), ctx, CancellationToken::new()).await;

        // 工具返回值是终态 Assistant 的 content 原文
        assert_eq!(out.to_wire(), "探索完成");
        // 转发流有始有终：Started 夹住生命周期，中间事件、终态 Assistant、
        // 滞留 Title 依次到达，Ended 收尾
        let kinds = collect_forwarded_kinds(&mut fwd_rx);
        assert_eq!(
            kinds,
            vec![
                "child-started",
                "chunk",
                "assistant",
                "title",
                "child-ended"
            ]
        );
    }

    #[tokio::test]
    async fn cancel_path_drains_residual_interrupt() {
        // 滞留通道的中断终态（destroy_session 后仍在缓冲）
        let (child_tx, child_rx) = mpsc::unbounded_channel();
        child_tx.send(interrupt_event()).unwrap();
        drop(child_tx);

        let (fwd_tx, mut fwd_rx) = mpsc::unbounded_channel();
        let (_ops_guard, ctx) = fake_ops_ctx(child_rx, fwd_tx);

        let cancel = CancellationToken::new();
        cancel.cancel();

        let out = subagent_handler(valid_args(), ctx, cancel).await;

        assert_eq!(out.to_wire(), "⚠️ 子代理被父取消");
        // 滞留的 Interrupt 终态经排空转发（前端子会话运行态据此回 idle）
        let kinds = collect_forwarded_kinds(&mut fwd_rx);
        assert_eq!(kinds, vec!["child-started", "interrupt", "child-ended"]);
    }
}
