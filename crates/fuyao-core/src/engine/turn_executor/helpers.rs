//! TurnExecutor 辅助方法
//!
//! 从 react_loop 中抽出的可独立测试的逻辑：
//! - emit_event：统一事件发送（走 dispatch 管道）
//! - prepare_request：构造 LLM 请求（before_llm hook + 工具 + 上下文）
//! - handle_stop：无工具调用时的最终消息发送
//! - consume_next_message：从队列取下一条消息并 deliver，保证 LLM 调用前 session_mgr 已记录
//! - drain_pending_to_guide：把 pending_queue 全部转移到 guide_queue（调度层操作）

use crate::llm::event_builder;
use crate::llm::stream_session;
use fuyao_api::AgentContext;
use fuyao_api::message::output::{QueueUpdateMessage, QueueUpdatePayload};
use fuyao_api::message::{EventBase, OutputEvent, QueueUpdateKind};
use fuyao_provider::ChatMessage;
use fuyao_provider::ChatRequest;
use std::collections::HashMap;

use super::TurnExecutor;

impl TurnExecutor {
    /// 统一事件发送：通过 dispatch 管道（拦截 → 发送 → 观察）
    pub(super) async fn emit_event(&self, event: fuyao_api::message::OutputEvent) {
        crate::dispatch::dispatch(event, None, &self.emitter).await;
    }

    /// 准备 LLM 请求：通过 hook 获取消息列表，构建 ChatRequest
    ///
    /// skip_tools 为 true 时返回空工具（压缩轮次等场景，防止 AI 调用工具）。
    pub(super) async fn prepare_request(
        &self,
    ) -> (
        ChatRequest,
        HashMap<String, fuyao_api::ToolFn>,
        Vec<serde_json::Value>,
        AgentContext,
    ) {
        // before_llm：获取消息列表 + skip_tools 信号
        let (messages, skip_tools) = {
            let mut hooks = self.emitter.hooks().lock().await;
            let output = hooks.hook_before_llm().await;
            let msgs = output
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
            (msgs, output.skip_tools)
        };

        let request = ChatRequest {
            messages,
            system: None,
        };
        // skip_tools：压缩轮次等场景不传工具给 LLM
        let (tools_handlers, tools_schema) = if skip_tools {
            (HashMap::new(), Vec::new())
        } else {
            let tools = self.tools.lock().expect("工具注册表锁异常");
            (tools.0.clone(), tools.1.clone())
        };
        let agent_ctx = self.agent_ctx.lock().expect("Agent 上下文锁异常").clone();

        (request, tools_handlers, tools_schema, agent_ctx)
    }

    /// 无工具调用：发出最终 AssistantMessage → 结束轮次
    pub(super) async fn handle_stop(&self, result: &stream_session::StreamResult) {
        let has_content = !result.text.is_empty() || !result.reasoning.is_empty();
        if has_content {
            event_builder::emit_assistant_message(
                &self.emitter,
                &result.text,
                &result.reasoning,
                &[],
                "stop",
                &result.usage,
            )
            .await;
        }
    }

    /// 从 guide_queue 取出下一条消息并 deliver（发送 + 触发观察钩子）
    ///
    /// 用于 ReAct 循环中"队列有新消息"时主动消费，避免消息卡在队列导致死循环。
    /// 返回 true 表示消费到消息（已 deliver），false 表示队列空。
    pub(super) async fn consume_next_message(&self) -> bool {
        let next = self.guide_queue.lock().expect("引导队列锁异常").pop_front();
        if let Some(queued) = next {
            let msg_base = queued.message.base.clone();
            crate::dispatch::deliver(&self.emitter, OutputEvent::User(queued.message)).await;
            // 通知 UI 队列长度变化（消费事件，复用 base.id）
            let guide_count = self.guide_queue.lock().expect("引导队列锁异常").len();
            let pending_count = self.pending_queue.lock().expect("排队队列锁异常").len();
            let _ = self
                .emitter
                .send(OutputEvent::QueueUpdate(QueueUpdateMessage {
                    base: msg_base,
                    payload: QueueUpdatePayload {
                        guide_count,
                        pending_count,
                        kind: QueueUpdateKind::Consumed,
                    },
                }))
                .await;
            true
        } else {
            false
        }
    }

    /// 把 pending_queue 全部转移到 guide_queue
    ///
    /// 在 `run()` 主循环顶部调用，统一处理 pending→guide 转移时机，覆盖三种场景：
    /// 1. AI 空闲 + 用户发 Pending（根本没进 run_turn）
    /// 2. 正常最终回复完成后（run_turn 退出后由 run() 顶部接管）
    /// 3. 中断退出 run_turn 后 pending 残留
    ///
    /// 幂等：pending 为空时立即返回，无副作用。
    /// 锁顺序：先 pending 后 guide，与 InputDispatcher 单锁不冲突，无死锁风险。
    pub(super) async fn drain_pending_to_guide(&self) {
        // 锁顺序：先 pending 后 guide，与 InputDispatcher 单锁不冲突，无死锁风险
        // 用块作用域确保锁在 await 之前释放（async 中 drop 不可靠）
        {
            let mut pending = self.pending_queue.lock().expect("排队队列锁异常");
            if pending.is_empty() {
                return;
            }
            let mut guide = self.guide_queue.lock().expect("引导队列锁异常");
            while let Some(q) = pending.pop_front() {
                guide.push_back(q);
            }
        }

        // 通知 UI 队列长度变化（转移事件，批量操作 base.id 用默认值）
        let guide_count = self.guide_queue.lock().expect("引导队列锁异常").len();
        let pending_count = self.pending_queue.lock().expect("排队队列锁异常").len();
        let _ = self
            .emitter
            .send(OutputEvent::QueueUpdate(QueueUpdateMessage {
                base: EventBase::default(),
                payload: QueueUpdatePayload {
                    guide_count,
                    pending_count,
                    kind: QueueUpdateKind::Transferred,
                },
            }))
            .await;
    }
}
