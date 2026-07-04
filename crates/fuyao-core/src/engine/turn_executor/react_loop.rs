//! ReAct 循环核心：TurnExecutor 的主循环与单轮执行
//!
//! 这是引擎的"心脏"：
//! - `run`：队列驱动主循环（空队列时 select! 等待）
//! - `run_turn`：单轮 ReAct 循环（stream + tool 执行 + 中断 select!）

use crate::engine::types::TurnCommand;
use crate::interrupt::{self, StreamAccumulator, StreamPhase};
use crate::llm::stream_session;
use crate::tool_runner;
use fuyao_api::message::{EventBase, OutputEvent, QueueUpdateData, QueueUpdateKind, TurnStartData};
use std::sync::{Arc, Mutex};

use super::TurnExecutor;
use super::outcome::{Phase, StreamOutcome, ToolExecOutcome};

impl TurnExecutor {
    /// 队列驱动主循环：从 guide_queue 取消息执行 ReAct 循环
    ///
    /// 队列空时阻塞等待 notify 或命令（Interrupt/Stop）。
    ///
    /// 取出消息时立即 deliver（发送 + 触发观察钩子），
    /// 保证 session_mgr 观察顺序为 [用户消息, AI回复] 严格交替。
    ///
    /// 每次循环顶部统一执行 pending→guide 转移（覆盖 AI 空闲、正常完成、中断三种场景）。
    pub(crate) async fn run(&mut self) {
        loop {
            // 顶部统一转移 pending→guide，覆盖三种场景：
            // 1. AI 空闲 + 用户发 Pending（根本没进 run_turn）
            // 2. 正常最终回复完成后（run_turn 退出后由这里接管）
            // 3. 中断退出 run_turn 后 pending 残留
            self.drain_pending_to_guide().await;

            // 从引导队列取一条消息
            let msg = {
                let mut q = self.guide_queue.lock().expect("引导队列锁异常");
                q.pop_front()
            };

            match msg {
                Some(queued) => {
                    // 取出时 deliver：发送 UserMessage 到 CLI + 触发插件观察
                    let msg_base = queued.message.base.clone();
                    crate::dispatch::deliver(
                        &self.emitter,
                        fuyao_api::message::OutputEvent::UserMessage(queued.message),
                    )
                    .await;
                    // 通知 UI 队列长度变化（消费事件，复用 base.id 让 UI 知道哪条消息被消费了）
                    let guide_count = self.guide_queue.lock().expect("引导队列锁异常").len();
                    let pending_count = self.pending_queue.lock().expect("排队队列锁异常").len();
                    let _ = self
                        .emitter
                        .send(OutputEvent::QueueUpdate(QueueUpdateData {
                            base: msg_base,
                            guide_count,
                            pending_count,
                            kind: QueueUpdateKind::Consumed,
                        }))
                        .await;
                    // 用该消息跑一轮 ReAct 循环
                    self.run_turn().await;
                }
                None => {
                    // 队列空：等待唤醒或命令
                    tokio::select! {
                        () = self.queue_notify.notified() => {}
                        cmd = self.rx_command.as_mut().expect("命令通道已丢失").recv() => {
                            match cmd {
                                Some(TurnCommand::Interrupt(_data)) => {
                                    // 无活跃轮次时的中断
                                    interrupt::idle::handle().await;
                                }
                                Some(TurnCommand::Stop) | None => break,
                            }
                        }
                    }
                }
            }
        }
    }

    /// 运行一个轮次（ReAct 循环），在关键 await 点使用 select! 监听中断
    ///
    /// 采用 Option dance 模式：临时 take 出 rx_command，
    /// 使 select! 可以同时持有 rx_command 和 &self 其他字段的引用。
    async fn run_turn(&mut self) {
        let mut rx = self.rx_command.take().expect("命令通道已被取走");

        // 发出轮次开始事件，供插件重置轮次级状态
        let _ = self
            .emit_event(OutputEvent::TurnStart(TurnStartData {
                base: EventBase::default(),
            }))
            .await;

        // 共享累积器：stream_session 写入，中断时读取部分结果
        let accumulator = Arc::new(Mutex::new(StreamAccumulator::new()));

        'react: loop {
            // 检查是否已有待处理的命令（上一轮中断可能残留）
            if let Ok(cmd) = rx.try_recv() {
                match cmd {
                    TurnCommand::Interrupt(_data) => {
                        // 中断输出事件已由 InputDispatcher dispatch 管道发出
                        interrupt::idle::handle().await;
                        break;
                    }
                    TurnCommand::Stop => {
                        self.rx_command = Some(rx);
                        return;
                    }
                }
            }

            let (request, tools_handlers, tools_schema, agent_ctx) = self.prepare_request().await;

            // 重置累积器
            accumulator.lock().expect("流式累积器锁异常").clear();

            // ── 流式会话 + 中断 select! ──
            let stream_outcome = tokio::select! {
                result = stream_session::run_stream_session(
                    self.provider.as_ref(),
                    request,
                    &self.agent_ctx,
                    if tools_schema.is_empty() { None } else { Some(tools_schema) },
                    &self.emitter,
                    Some(accumulator.clone()),
                ) => {
                    StreamOutcome::Completed(result)
                }
                cmd = rx.recv() => {
                    match cmd {
                        Some(TurnCommand::Interrupt(data)) => StreamOutcome::Interrupted(data),
                        Some(TurnCommand::Stop) | None => {
                            self.rx_command = Some(rx);
                            return;
                        }
                    }
                }
            };

            match stream_outcome {
                StreamOutcome::Completed(result) => {
                    let result = match result {
                        Ok(r) => r,
                        Err(_) => break,
                    };

                    let has_tool_calls = !result.tool_calls.is_empty();

                    if has_tool_calls {
                        // 发出 AssistantMessage（含工具调用）
                        crate::llm::event_builder::emit_assistant_message(
                            &self.emitter,
                            &result.text,
                            &result.reasoning,
                            &result.tool_calls,
                            "tool_calls",
                            &result.usage,
                        )
                        .await;

                        // ── 工具执行 + 中断 select! ──
                        let tool_outcome = tokio::select! {
                            _ = tool_runner::orchestrate(
                                &result.tool_calls,
                                &tools_handlers,
                                &agent_ctx,
                                &self.emitter,
                            ) => {
                                ToolExecOutcome::Completed
                            }
                            cmd = rx.recv() => {
                                match cmd {
                                    Some(TurnCommand::Interrupt(data)) => {
                                        ToolExecOutcome::Interrupted(data)
                                    }
                                    Some(TurnCommand::Stop) | None => {
                                        self.rx_command = Some(rx);
                                        return;
                                    }
                                }
                            }
                        };

                        match tool_outcome {
                            // 工具执行完成：工具结果是 ReAct 中间产物，LLM 必须看到并决定下一步
                            // 队列里若有新消息（用户在工具执行时补充），先 deliver 让 LLM 一起看到
                            ToolExecOutcome::Completed => {
                                self.consume_next_message().await;
                                continue 'react;
                            }
                            ToolExecOutcome::Interrupted(data) => {
                                interrupt::tool_exec::handle(
                                    &self.emitter,
                                    data,
                                    &result.tool_calls,
                                )
                                .await;
                                break;
                            }
                        }
                    } else {
                        // 无工具调用：发出最终 AssistantMessage
                        self.handle_stop(&result).await;

                        // 尝试消费下一条消息：有则继续 ReAct，空则停止
                        // pending→guide 转移已由 run() 主循环顶部统一处理
                        if self.consume_next_message().await {
                            continue 'react;
                        } else {
                            break;
                        }
                    }
                }
                StreamOutcome::Interrupted(data) => {
                    // 先克隆数据再释放锁，避免 MutexGuard 跨 await（不 Send）
                    let (phase, text, reasoning, tool_calls, usage) = {
                        let acc = accumulator.lock().expect("流式累积器锁异常");
                        let phase = match acc.phase {
                            StreamPhase::Streaming => Phase::Streaming,
                            StreamPhase::Backoff => Phase::Backoff,
                        };
                        (
                            phase,
                            acc.text.clone(),
                            acc.reasoning.clone(),
                            acc.tool_calls.clone(),
                            acc.usage.clone(),
                        )
                    };

                    match phase {
                        Phase::Backoff => {
                            // 重试/退避期间中断：无增量内容需要保存
                            interrupt::retry_backoff::handle().await;
                        }
                        Phase::Streaming if !tool_calls.is_empty() => {
                            // LLM 工具调用流中中断
                            interrupt::llm_toolcall::handle(
                                &self.emitter,
                                data,
                                &text,
                                &reasoning,
                                &tool_calls,
                                &usage,
                            )
                            .await;
                        }
                        Phase::Streaming => {
                            // LLM 流式输出中中断
                            interrupt::llm_output::handle(
                                &self.emitter,
                                data,
                                &text,
                                &reasoning,
                                &usage,
                            )
                            .await;
                        }
                    }
                    break;
                }
            }
        }

        self.rx_command = Some(rx);
    }
}
