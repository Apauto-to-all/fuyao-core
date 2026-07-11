//! Session 钩子注册
//!
//! SessionHooksState 持有 tx_send（发送插件通知）、agent_ctx（同步 session_id）、
//! 压缩追踪器（阈值检测 + split_session）。
//! SessionPlugin 将 session 管理的核心钩子注册到 HooksRegistry。
//! 与 loop_guard 的 LoopGuardState 模式一致。

use super::session_context::SessionContext;
use crate::compressor::tracker::CompressionTracker;
use crate::compressor::{COMPRESSION_SYSTEM_PROMPT, expand_for_integrity};
use crate::title_generator::maybe_generate_title;
use fuyao_api::message::input::{
    PluginSource, UserMessage, UserMessageMode, UserMessageSource, UserPayload,
};
use fuyao_api::message::output::AssistantMessage;
use fuyao_api::message::{EventBase, InputEvent, OutputEvent};
use fuyao_api::{AgentPaths, Session, SharedAgentCtx};
use fuyao_hooks::{BeforeLlmOutput, Plugin, PluginEmitter, SharedHooks};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Session 钩子层内部状态
///
/// 持有 tx_send（发送插件通知）、agent_ctx（同步 session_id）、
/// 压缩追踪器（阈值检测 + split_session），
/// 与 loop_guard 的 LoopGuardState 模式一致。
struct SessionHooksState {
    /// 插件消息发送器（由 SendInputFn 回调时设置，绑定身份发 Plugin 消息 + 复用通道发 User 引导消息）
    emitter: Option<PluginEmitter>,
    /// Agent 运行上下文引用（用于同步 session_id）
    agent_ctx: SharedAgentCtx,
    /// Session 上下文引用
    session_ctx: Arc<Mutex<SessionContext>>,
    /// 压缩进行中标记
    compression_in_progress: bool,
    /// 压缩触发追踪器
    tracker: CompressionTracker,
}

impl SessionHooksState {
    fn new(agent_ctx: SharedAgentCtx, session_ctx: Arc<Mutex<SessionContext>>) -> Self {
        let mut tracker = CompressionTracker::new();
        {
            let ctx = agent_ctx.lock().unwrap_or_else(|e| e.into_inner());
            tracker.set_agent_paths(ctx.agent_paths.clone());
        }
        Self {
            emitter: None,
            agent_ctx,
            session_ctx,
            compression_in_progress: false,
            tracker,
        }
    }

    /// 设置插件消息发送器（由 SendInputFn 回调时调用）
    fn set_emitter(&mut self, emitter: PluginEmitter) {
        self.emitter = Some(emitter);
    }

    /// 获取初始 session_id（从 agent_ctx 读取）
    fn initial_session_id(&self) -> Option<String> {
        match self.agent_ctx.lock() {
            Ok(ac) => ac.session_id.clone(),
            Err(_) => {
                tracing::warn!("Agent 上下文锁中毒，跳过 session_id 读取");
                None
            }
        }
    }

    /// 统一更新 session_id，同步到 AgentContext 并发送插件通知
    fn sync_session_id(&self, new_id: String) {
        let old_id = {
            let mut agent_ctx = match self.agent_ctx.lock() {
                Ok(ctx) => ctx,
                Err(_) => {
                    tracing::warn!("Agent 上下文锁中毒，跳过 session_id 同步");
                    return;
                }
            };
            let old = agent_ctx.session_id.clone();
            agent_ctx.session_id = Some(new_id.clone());
            old
        };

        // 切换 session 时通知外部
        if let Some(ref old) = old_id
            && old != &new_id
        {
            self.emit_plugin(
                "session_switched",
                &format!(
                    "Session 切换: {}... → {}...",
                    &old[..8.min(old.len())],
                    &new_id[..8.min(new_id.len())]
                ),
            );
        }
    }

    /// 发送插件通知事件（委托 emitter，身份绑定 = Plugin::name()）
    fn emit_plugin(&self, event_type: &str, message: &str) {
        if let Some(ref e) = self.emitter {
            e.emit_message(event_type, message);
        }
    }

    /// 发送累积统计插件事件（供 TUI 统计栏消费）
    fn emit_cumulative_stats(&self, session: &Session) {
        if let Some(ref e) = self.emitter {
            let cost = session.messages.last().map(|m| m.cost).unwrap_or(0.0);
            e.emit_data(
                "cumulative_stats",
                serde_json::json!({
                    "message_count": session.message_count,
                    "tool_call_count": session.tool_call_count,
                    "total_prompt_tokens": session.total_prompt_tokens,
                    "total_completion_tokens": session.total_completion_tokens,
                    "total_reasoning_tokens": session.total_reasoning_tokens,
                    "total_cached_tokens": session.total_cached_tokens,
                    "cost": cost,
                    "total_cost": session.total_cost,
                }),
            );
        }
    }

    /// 消息持久化后，尝试发射累积统计
    async fn on_output_stats(&self, session_ctx: &SessionContext) {
        if let (Some(session_id), Some(mgr)) =
            (&session_ctx.session_id, &session_ctx.session_manager)
            && let Ok(Some(session)) = mgr.get(session_id).await
        {
            self.emit_cumulative_stats(&session);
        }
    }

    /// 注入压缩引导消息到引擎 Guide 队列
    fn inject_compression_guide(&mut self) {
        if let Some(ref e) = self.emitter {
            let send_result = e.sender().try_send(InputEvent::User(UserMessage {
                base: EventBase::default(),
                payload: UserPayload {
                    content: COMPRESSION_SYSTEM_PROMPT.to_string(),
                    mode: UserMessageMode::Guide,
                    source: UserMessageSource::Plugin(PluginSource {
                        name: "compressor".to_string(),
                    }),
                },
            }));
            if send_result.is_err() {
                tracing::warn!("压缩引导消息入队失败");
            } else {
                self.compression_in_progress = true;
            }
        }
    }

    /// 压缩完成处理：构造摘要消息 + 保留窗口 → split_session → 同步 session_id
    ///
    /// 窗口逻辑：
    /// 1. 定位压缩引导消息（Plugin("compressor") 的 user 消息）位置
    /// 2. 引导消息之前的消息 = 原始对话，计算保留窗口
    /// 3. expand_for_integrity 确保块完整
    /// 4. 新 session = [摘要消息] + [保留窗口消息]
    async fn handle_compression_complete(&mut self, data: &AssistantMessage) {
        self.compression_in_progress = false;

        let summary_content = match data.payload.content.as_ref() {
            Some(content) if !content.is_empty() => content.clone(),
            _ => return,
        };

        let (agent_paths, agent_config) = {
            let agent_ctx = self.agent_ctx.lock().unwrap_or_else(|e| e.into_inner());
            (
                agent_ctx.agent_paths.clone(),
                agent_ctx.agent_config.clone(),
            )
        };

        let mut ctx = self.session_ctx.lock().await;

        let Some(session_id) = ctx.session_id_cloned() else {
            return;
        };

        // 构造摘要消息
        let mut summary_msg =
            fuyao_api::Message::assistant(Some(format!("[对话摘要]\n{summary_content}")));
        summary_msg.reasoning = data.payload.reasoning.clone();
        summary_msg.finish_reason = data.payload.finish_reason.clone();
        summary_msg.prompt_tokens = data.payload.prompt_tokens;
        summary_msg.completion_tokens = data.payload.completion_tokens;
        summary_msg.reasoning_tokens = data.payload.reasoning_tokens;
        summary_msg.cached_tokens = data.payload.cached_tokens;

        // 获取当前消息列表
        let all_messages = ctx.get_messages();

        // 定位压缩引导消息位置
        let guide_idx = all_messages.iter().rposition(|m| {
            m.role == "user" && m.content.as_deref() == Some(COMPRESSION_SYSTEM_PROMPT)
        });

        // 计算保留窗口：引导消息之前的消息是原始对话
        let recent_messages = match guide_idx {
            Some(guide) if guide > 0 => {
                let original = &all_messages[..guide];
                // recent_window 从全局配置读取；保留窗口 = min(window, 原始消息数 / window)
                let recent_window = fuyao_api::get_config().session.compression.recent_window;
                let recent_window =
                    std::cmp::min(recent_window, original.len() / recent_window.max(1));
                if recent_window == 0 {
                    // 消息太少，不保留额外窗口
                    Vec::new()
                } else {
                    let recent_start = original.len() - recent_window;
                    let recent_start = expand_for_integrity(original, recent_start);
                    original[recent_start..].to_vec()
                }
            }
            _ => Vec::new(),
        };

        // 新 session 消息 = [摘要消息] + [保留窗口消息]
        let mut compressed_messages = vec![summary_msg];
        compressed_messages.extend(recent_messages);

        // 生成新系统提示词
        let new_system_prompt = fuyao_prompt::build_system_prompt(&agent_paths, &agent_config);

        // 执行 split_session
        let Some(ref mgr) = ctx.session_manager else {
            return;
        };

        match mgr
            .split_session(&session_id, new_system_prompt, compressed_messages, None)
            .await
        {
            Ok(new_session) => {
                let new_id = new_session.id;
                tracing::info!(
                    old_session_id = %session_id,
                    new_session_id = %new_id,
                    message_count = new_session.messages.len(),
                    "压缩完成，已切换到新会话"
                );
                self.sync_session_id(new_id.clone());
                if let Err(e) = ctx.switch_session(new_id).await {
                    tracing::warn!(cause = %e, "压缩后切换会话上下文失败");
                }
            }
            Err(e) => {
                tracing::warn!(session_id = %session_id, cause = %e, "压缩分裂失败，保留旧会话继续对话");
            }
        }
    }

    /// 首轮对话后自动重命名（fire-and-forget）
    ///
    /// 仅在首轮（user 消息数 == 1）触发，spawn 异步生成标题并写回 DB。
    /// 生成失败或非首轮则跳过，不阻塞主对话。标题模型优先级：fast → 当前引擎模型 → 放弃。
    async fn try_auto_title(&self, assistant_data: &AssistantMessage) {
        // 开关关闭时直接跳过
        if !fuyao_api::get_config().session.title.enabled {
            return;
        }
        // 一次锁 session_ctx：取 user 消息 + session_manager
        let (user_msg, mgr) = {
            let ctx = self.session_ctx.lock().await;
            let messages = ctx.get_messages();
            let user_count = messages.iter().filter(|m| m.role == "user").count();
            if user_count != 1 {
                return;
            }
            let user_msg = messages
                .iter()
                .find(|m| m.role == "user")
                .and_then(|m| m.content.clone());
            (user_msg, ctx.session_manager.clone())
        };
        let Some(user_msg) = user_msg else {
            return;
        };
        let Some(mgr) = mgr else {
            return;
        };
        let assistant_msg = assistant_data.payload.content.clone().unwrap_or_default();
        // 从 agent_ctx 取 session_id + model_id + agent_paths（一次锁取）
        let (session_id, model_id, agent_paths) = {
            let g = self.agent_ctx.lock().unwrap_or_else(|e| e.into_inner());
            (
                g.session_id.clone(),
                g.model_config.model_id.clone(),
                g.agent_paths.clone(),
            )
        };
        let (Some(sid), Some(mid)) = (session_id, model_id) else {
            return;
        };
        // fire-and-forget：不阻塞 hook，生成失败静默放弃
        tokio::spawn(async move {
            match maybe_generate_title(&user_msg, &assistant_msg, &mid, &agent_paths).await {
                Some(title) => match mgr.set_session_title(&sid, &title).await {
                    Ok(true) => {
                        tracing::info!(session_id = %sid, title = %title, "会话自动重命名完成");
                    }
                    Ok(false) => {
                        tracing::debug!(session_id = %sid, "会话不存在，跳过重命名");
                    }
                    Err(e) => tracing::warn!(cause = %e, "自动重命名写入失败"),
                },
                None => tracing::debug!("会话自动重命名跳过（生成失败）"),
            }
        });
    }
}

/// Session 管理插件
///
/// 通过 before_llm（消息列表 + ensure_session 懒初始化）+ output_observe（持久化 + 统计 + 压缩检测）+
/// send_input（获取 tx_send）三个钩子实现会话管理。
///
/// 与 loop_guard 的 LoopGuardPlugin 架构一致。
pub struct SessionPlugin {
    state: Arc<Mutex<SessionHooksState>>,
}

impl SessionPlugin {
    /// 构造 session 管理插件
    ///
    /// 内部自行构造 [`SessionContext`]（会话级内存态），不向外部暴露——
    /// SessionContext 仅供插件 hook 内部消费（喂 LLM / 持久化 / 压缩），无跨模块读需求。
    ///
    /// # 参数
    /// - `agent_paths`: Agent 路径配置（用于构造 SessionContext，定位 sessions_db）
    /// - `agent_ctx`: Agent 运行上下文（同步 session_id / 读 model_id / 读 agent_config）
    pub fn new(agent_paths: AgentPaths, agent_ctx: SharedAgentCtx) -> Self {
        let agent_config = {
            let ctx = agent_ctx.lock().unwrap_or_else(|e| e.into_inner());
            ctx.agent_config.clone()
        };
        let session_ctx = Arc::new(Mutex::new(SessionContext::new(agent_paths, agent_config)));
        Self {
            state: Arc::new(Mutex::new(SessionHooksState::new(agent_ctx, session_ctx))),
        }
    }
}

#[async_trait::async_trait]
impl Plugin for SessionPlugin {
    fn name(&self) -> &str {
        "session"
    }

    async fn register(&self, hooks: &SharedHooks) {
        // hook: before_llm → 返回消息列表（含 ensure_session 懒初始化）+ skip_tools 信号
        let s_before = self.state.clone();
        hooks.lock().await.register_before_llm(
            0,
            Arc::new(move || {
                let s = s_before.clone();
                Box::pin(async move {
                    let guard = s.lock().await;
                    let mut ctx = guard.session_ctx.lock().await;
                    let initial_id = guard.initial_session_id();

                    // 懒初始化 session
                    if let Err(e) = ctx.ensure_session(initial_id.as_deref()).await {
                        tracing::warn!(cause = %e, "before_llm 会话初始化失败，以空上下文调用 LLM");
                    }

                    if let Some(ref sid) = ctx.session_id {
                        guard.sync_session_id(sid.clone());
                    }
                    BeforeLlmOutput {
                        messages: ctx.get_messages(),
                        // 压缩进行中时跳过工具，防止 AI 在摘要时调用工具
                        skip_tools: guard.compression_in_progress,
                    }
                })
            }),
        );

        // hook: output_observe → 持久化消息 + 发射统计 + 压缩检测
        //
        // 严格串行：先持久化消息，再检测压缩。
        // 确保压缩触发时最新消息已持久化到 DB。
        let s_output = self.state.clone();
        hooks
            .lock()
            .await
            .register_output_observe(Arc::new(move |msg: OutputEvent| {
                let s = s_output.clone();
                Box::pin(async move {
                    // 阶段1：持久化消息 + 发射统计
                    let persisted = {
                        let guard = s.lock().await;
                        let mut ctx = guard.session_ctx.lock().await;
                        let persisted = ctx.on_output(msg.clone()).await;
                        if persisted {
                            guard.on_output_stats(&ctx).await;
                        }
                        persisted
                    };

                    // 阶段2：压缩逻辑（仅处理 Assistant 事件且已持久化）
                    if !persisted {
                        return;
                    }
                    if let OutputEvent::Assistant(ref data) = msg {
                        let mut guard = s.lock().await;
                        // 正在压缩中 → 检查是否是摘要响应（无工具调用）
                        if guard.compression_in_progress {
                            let has_tool_calls = data
                                .payload
                                .tool_calls
                                .as_ref()
                                .is_some_and(|tc| !tc.is_empty());
                            if !has_tool_calls {
                                guard.handle_compression_complete(data).await;
                            }
                            return;
                        }

                        // 非压缩中 → 检测阈值，决定是否触发压缩
                        let prompt_tokens = data.payload.prompt_tokens as usize;
                        // 从 agent_ctx 读取当前运行时 model_id + session_id（一次锁取）
                        let (model_id, session_id) = {
                            let g = guard.agent_ctx.lock().unwrap_or_else(|e| e.into_inner());
                            (g.model_config.model_id.clone(), g.session_id.clone())
                        };
                        let threshold = fuyao_api::get_config().session.compression.threshold;
                        if guard
                            .tracker
                            .should_compress(prompt_tokens, model_id.as_deref())
                        {
                            tracing::warn!(
                                usage = prompt_tokens,
                                threshold = threshold,
                                session_id = ?session_id,
                                "上下文压缩触发"
                            );
                            guard.inject_compression_guide();
                        }

                        // 首轮后尝试自动重命名（fire-and-forget，不阻塞）
                        guard.try_auto_title(data).await;
                    }
                })
            }));

        // hook: send_input → 用插件身份构造 emitter 注入 state，后续发 Plugin 通知和压缩引导消息
        let s_send = self.state.clone();
        let identity = self.identity();
        let send_fn: fuyao_hooks::SendInputFn = Arc::new(move |tx| {
            let s = s_send.clone();
            let id = identity.clone();
            Box::pin(async move {
                let emitter = PluginEmitter::new(id, tx);
                s.lock().await.set_emitter(emitter);
            })
        });
        hooks.lock().await.register_send_input(0, send_fn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::AgentConfig;
    use fuyao_api::AgentPaths;
    use fuyao_api::message::input::PluginEventSource;

    #[test]
    fn session_hooks_state_new_initializes_tracker() {
        let agent_ctx = Arc::new(std::sync::Mutex::new(fuyao_api::AgentContext {
            agent_paths: AgentPaths::default(),
            ..Default::default()
        }));
        let session_ctx = Arc::new(Mutex::new(SessionContext::new(
            AgentPaths::default(),
            AgentConfig::default(),
        )));
        let state = SessionHooksState::new(agent_ctx, session_ctx);
        assert!(!state.compression_in_progress);
        assert!(!state.tracker.should_compress(100_000, None));
        assert!(state.tracker.should_compress(110_000, None));
    }

    #[tokio::test]
    async fn inject_compression_guide_sends_message() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<InputEvent>(10);
        let agent_ctx = Arc::new(std::sync::Mutex::new(fuyao_api::AgentContext::default()));
        let session_ctx = Arc::new(Mutex::new(SessionContext::new(
            AgentPaths::default(),
            AgentConfig::default(),
        )));
        let mut state = SessionHooksState::new(agent_ctx, session_ctx);
        state.set_emitter(PluginEmitter::new(
            PluginEventSource {
                name: "session".into(),
            },
            tx,
        ));

        assert!(!state.compression_in_progress);
        state.inject_compression_guide();
        assert!(state.compression_in_progress);

        let event = rx.try_recv().unwrap();
        match event {
            InputEvent::User(data) => {
                assert_eq!(data.payload.mode, UserMessageMode::Guide);
                assert_eq!(
                    data.payload.source,
                    UserMessageSource::Plugin(PluginSource {
                        name: "compressor".to_string(),
                    })
                );
                assert_eq!(data.payload.content, COMPRESSION_SYSTEM_PROMPT);
            }
            _ => panic!("应为 User 事件"),
        }
    }

    #[tokio::test]
    async fn inject_compression_guide_no_panic_without_sender() {
        let agent_ctx = Arc::new(std::sync::Mutex::new(fuyao_api::AgentContext::default()));
        let session_ctx = Arc::new(Mutex::new(SessionContext::new(
            AgentPaths::default(),
            AgentConfig::default(),
        )));
        let mut state = SessionHooksState::new(agent_ctx, session_ctx);

        // 没有 tx_send，inject 不应该 panic
        state.inject_compression_guide();
        assert!(!state.compression_in_progress);
    }
}
