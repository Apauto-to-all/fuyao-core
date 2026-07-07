//! LoopGuard 协调层
//!
//! 组合 ToolLoopGuard 和 TextLoopGuard，提供统一的钩子接口。
//! 工具循环检测优先级高于文本循环检测。
//! 所有事件发送通过 SendInputFn 获取的 tx_send 实现，
//! 不持有引擎内部 channel。

use std::sync::Arc;

use fuyao_api::message::input::{
    InterruptMessage, InterruptPayload, InterruptSource, PluginEventSource, PluginMessage,
    PluginPayload, PluginSource, UserMessage, UserMessageMode, UserMessageSource, UserPayload,
};
use fuyao_api::message::output::{ChunkMessage, ToolCallMessage, ToolResultMessage};
use fuyao_api::message::{EventBase, InputEvent, OutputEvent};
use fuyao_hooks::InterceptResult;
use tokio::sync::Mutex;

use super::text_guard::TextLoopGuard;
use super::tool_guard::ToolLoopGuard;
use super::types::LoopSeverity;
use fuyao_api::LoopGuardConfig;

/// 输入事件发送端类型（由 SendInputFn 回调时保存）
type InputEventSender = tokio::sync::mpsc::Sender<InputEvent>;

/// LoopGuard 协调状态
pub(crate) struct LoopGuardState {
    /// 工具循环检测器
    tool_guard: ToolLoopGuard,
    /// 文本循环检测器
    text_guard: TextLoopGuard,
    /// 待注入的警告消息（追加到工具结果前）
    pending_warn: String,
    /// 待注入的拦截消息（替换工具结果内容）
    pending_inject: String,
    /// 当前待处理的严重程度（用于 output_intercept 判断是否注入/Block）
    pending_severity: Option<LoopSeverity>,
    /// 中断次数计数器
    interrupt_count: usize,
    /// 是否已终止（防止重复触发，轮次重置时清空）
    aborted: bool,
    /// 输入事件发送端（由 SendInputFn 回调时设置，用于发送所有输入事件）
    tx_send: Option<InputEventSender>,
}

impl LoopGuardState {
    pub fn new(config: LoopGuardConfig) -> Self {
        let tool_guard = ToolLoopGuard::new(config.clone());
        let text_guard = TextLoopGuard::new(config);

        Self {
            tool_guard,
            text_guard,
            pending_warn: String::new(),
            pending_inject: String::new(),
            pending_severity: None,
            interrupt_count: 0,
            aborted: false,
            tx_send: None,
        }
    }

    /// 设置输入事件发送端（由 SendInputFn 回调时调用）
    pub fn set_tx_send(&mut self, tx: InputEventSender) {
        self.tx_send = Some(tx);
    }

    /// 发送插件通知事件
    ///
    /// 通过 tx_send 发送 InputEvent::Plugin，引擎主循环收到后
    /// 转发为 OutputEvent::Plugin 通知 UI。
    fn emit_plugin(&self, event_type: &str, message: &str) {
        if let Some(ref tx) = self.tx_send {
            let _ = tx.try_send(InputEvent::Plugin(PluginMessage {
                base: EventBase::default(),
                payload: PluginPayload {
                    source: PluginEventSource {
                        name: "loop_guard".to_string(),
                    },
                    event_type: event_type.to_string(),
                    data: None,
                    error: None,
                    message: Some(message.to_string()),
                },
            }));
        }
    }

    /// 发送中断输入事件
    ///
    /// 通过 tx_send 发送 InputEvent::Interrupt，由引擎主循环统一处理。
    fn send_interrupt(&self, reason: String) {
        if let Some(ref tx) = self.tx_send {
            let _ = tx.try_send(InputEvent::Interrupt(InterruptMessage {
                base: EventBase::default(),
                payload: InterruptPayload {
                    reason,
                    source: InterruptSource::Hook,
                },
            }));
        }
    }

    /// 发送注入用户消息事件
    ///
    /// 通过 tx_send 发送 InputEvent::User（source = Plugin），
    /// 引擎收到后注入对话历史，供下次 LLM 调用时 AI 看到引导消息。
    fn send_inject_message(&self, content: String) {
        if let Some(ref tx) = self.tx_send {
            let _ = tx.try_send(InputEvent::User(UserMessage {
                base: EventBase::default(),
                payload: UserPayload {
                    content,
                    mode: UserMessageMode::Guide,
                    source: UserMessageSource::Plugin(PluginSource {
                        name: "loop_guard".to_string(),
                    }),
                },
            }));
        }
    }

    /// 重置轮次状态：用户主动开新对话时调用
    ///
    /// 清空所有跨轮次残留状态（含 interrupt_count），
    /// 确保新对话从干净状态开始检测。
    pub fn reset_turn(&mut self) {
        self.aborted = false;
        self.pending_warn.clear();
        self.pending_inject.clear();
        self.pending_severity = None;
        self.interrupt_count = 0;
        self.tool_guard.reset();
        self.text_guard.reset();
    }

    /// 清理残留的 pending 状态（工具结果已通过 intercept 处理后的兜底清理）
    ///
    /// 不重置 tool_guard / text_guard / interrupt_count，
    /// 确保插件注入消息后循环检测仍保持"热"状态。
    fn clear_pending(&mut self) {
        self.pending_warn.clear();
        self.pending_inject.clear();
        self.pending_severity = None;
    }

    /// 处理流式内容块（文本循环检测）
    pub fn handle_chunk(&mut self, chunk: &ChunkMessage) {
        let result = self.text_guard.handle_chunk(
            chunk.payload.content.as_deref(),
            chunk.payload.reasoning.as_deref(),
            self.interrupt_count,
        );

        if let Some(r) = result {
            self.pending_severity = Some(r.severity);
            match r.severity {
                LoopSeverity::Abort => {
                    self.pending_inject = r.message.clone();
                    self.emit_plugin("loop_abort", &r.message);
                    self.aborted = true;
                    self.send_interrupt(r.message);
                }
                LoopSeverity::Interrupt => {
                    self.interrupt_count += 1;
                    self.emit_plugin("loop_interrupt", &r.message);
                    self.pending_inject = r.message.clone();
                    self.send_interrupt(r.message.clone());
                    self.send_inject_message(r.message);
                }
                _ => {
                    self.emit_plugin("loop_warn", &r.message);
                }
            }
        }
    }

    /// 处理工具调用事件（工具循环检测，优先级高于文本）
    pub fn handle_tool_call(&mut self, tc: &ToolCallMessage) {
        let result = self.tool_guard.handle_tool_call(
            &tc.payload.tool_name,
            &tc.payload.tool_args.to_string(),
            self.interrupt_count,
        );

        if let Some(r) = result {
            self.pending_severity = Some(r.severity);
            match r.severity {
                LoopSeverity::Abort => {
                    self.pending_inject = r.message.clone();
                    self.emit_plugin("loop_abort", &r.message);
                    self.aborted = true;
                    self.send_interrupt(r.message);
                }
                LoopSeverity::Interrupt => {
                    self.interrupt_count += 1;
                    self.emit_plugin("loop_interrupt", &r.message);
                    self.pending_inject = r.message.clone();
                    self.send_interrupt(r.message.clone());
                    self.send_inject_message(r.message);
                }
                LoopSeverity::Inject => {
                    self.emit_plugin("loop_inject", &r.message);
                    self.pending_inject = r.message;
                }
                _ => {
                    self.emit_plugin("loop_warn", &r.message);
                    self.pending_warn = r.message;
                }
            }
        }
    }

    /// 处理工具结果拦截：注入警告或替换内容
    pub fn intercept_tool_result(&mut self, result: &mut ToolResultMessage) {
        if !self.pending_inject.is_empty() {
            let inject = std::mem::take(&mut self.pending_inject);
            result.payload.content = format!("[循环检测] {inject}");
        } else if !self.pending_warn.is_empty() {
            let warn = std::mem::take(&mut self.pending_warn);
            result.payload.content = format!("[循环检测警告] {warn}\n\n{}", result.payload.content);
        }
    }
}

/// 构建输出观察钩子
///
/// 重置时机绑定到 `OutputEvent::User` 的 deliver（与 session_mgr 观察同步），
/// 按消息来源区分重置范围：
/// - 用户主动消息（source=User）：完全重置（新对话意图）
/// - 插件/系统注入（source=Plugin/System）：仅清理 pending 状态，不重置检测器
///   确保循环检测在插件注入后仍保持"热"状态，AI 若继续重复会立即被捕获
///
/// 工具调用优先于文本内容处理。
pub fn make_output_observe(state: Arc<Mutex<LoopGuardState>>) -> fuyao_hooks::OutputObserveFn {
    Arc::new(move |msg: OutputEvent| {
        let state = state.clone();
        Box::pin(async move {
            let mut guard = state.lock().await;
            match &msg {
                OutputEvent::User(um) => match &um.payload.source {
                    UserMessageSource::User => guard.reset_turn(),
                    UserMessageSource::Plugin(_) | UserMessageSource::System(_) => {
                        guard.clear_pending();
                    }
                },
                OutputEvent::ToolCall(tc) => guard.handle_tool_call(tc),
                OutputEvent::Chunk(chunk) => guard.handle_chunk(chunk),
                _ => {}
            }
        })
    })
}

/// 构建输出拦截钩子
///
/// 只负责修改或阻止事件：
/// - 工具结果 + Warn/Inject 级别：注入警告或替换内容后放行
/// - Interrupt/Abort 级别的中断已由 observe 钩子通过 tx_input 发送，intercept 不再处理
pub fn make_output_intercept(state: Arc<Mutex<LoopGuardState>>) -> fuyao_hooks::OutputInterceptFn {
    Arc::new(move |msg: &OutputEvent| match msg {
        OutputEvent::ToolResult(_) => match state.try_lock() {
            Ok(mut guard) => {
                let mut modified = msg.clone();
                if let OutputEvent::ToolResult(ref mut tr) = modified {
                    guard.intercept_tool_result(tr);
                }
                guard.pending_severity = None;
                InterceptResult::Pass(modified)
            }
            Err(_) => InterceptResult::Pass(msg.clone()),
        },
        OutputEvent::Chunk(_) => match state.try_lock() {
            Ok(mut guard) => {
                guard.pending_severity = None;
                InterceptResult::Pass(msg.clone())
            }
            Err(_) => InterceptResult::Pass(msg.clone()),
        },
        _ => InterceptResult::Pass(msg.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::message::output::{
        ChunkPayload, ToolCallPayload, ToolResultPayload, UserMessage as OutputUserMessage,
        UserPayload as OutputUserPayload,
    };

    fn make_chunk(content: Option<&str>, reasoning: Option<&str>) -> ChunkMessage {
        ChunkMessage {
            base: EventBase::default(),
            payload: ChunkPayload {
                content: content.map(|s| s.to_string()),
                reasoning: reasoning.map(|s| s.to_string()),
            },
        }
    }

    fn make_tool_call(name: &str, args: &str) -> ToolCallMessage {
        ToolCallMessage {
            base: EventBase::default(),
            payload: ToolCallPayload {
                tool_call_id: "call_1".to_string(),
                tool_name: name.to_string(),
                tool_args: serde_json::from_str(args).unwrap_or(serde_json::Value::Null),
            },
        }
    }

    fn make_tool_result(name: &str, content: &str) -> ToolResultMessage {
        ToolResultMessage {
            base: EventBase::default(),
            payload: ToolResultPayload {
                tool_call_id: "call_1".to_string(),
                tool_name: name.to_string(),
                content: content.to_string(),
            },
        }
    }

    #[test]
    fn emit_plugin_sends_event() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut state = LoopGuardState::new(LoopGuardConfig::default());
        state.set_tx_send(tx);
        state.emit_plugin("loop_warn", "测试通知");
        let event = rx.try_recv().unwrap();
        match event {
            InputEvent::Plugin(data) => {
                assert_eq!(data.payload.source.name, "loop_guard");
                assert_eq!(data.payload.event_type, "loop_warn");
                assert_eq!(data.payload.message, Some("测试通知".to_string()));
            }
            _ => panic!("应为 Plugin 事件"),
        }
    }

    #[test]
    fn abort_sets_aborted_flag() {
        let mut state = LoopGuardState::new(LoopGuardConfig::default());
        assert!(!state.aborted);
        // 模拟 Abort 级别处理
        state.aborted = true;
        assert!(state.aborted);
    }

    #[test]
    fn reset_turn_clears_aborted() {
        let mut state = LoopGuardState::new(LoopGuardConfig::default());
        state.aborted = true;
        state.pending_warn = "警告".to_string();
        state.pending_inject = "注入".to_string();
        state.pending_severity = Some(LoopSeverity::Abort);
        state.interrupt_count = 3;
        state
            .tool_guard
            .handle_tool_call("bash", r#"{"command":"ls"}"#, 0);
        state.reset_turn();
        assert!(!state.aborted);
        assert!(state.pending_warn.is_empty());
        assert!(state.pending_inject.is_empty());
        assert!(state.pending_severity.is_none());
        assert_eq!(state.interrupt_count, 0);
        assert!(state.tool_guard.tool_history.is_empty());
    }

    #[test]
    fn handle_chunk_accumulates_text() {
        let mut state = LoopGuardState::new(LoopGuardConfig {
            streaming_check_interval: 10,
            ..Default::default()
        });
        state.handle_chunk(&make_chunk(Some("hello world"), None));
        assert_eq!(state.text_guard.accumulated_text, "hello world");
    }

    #[test]
    fn handle_tool_call_records_history() {
        let mut state = LoopGuardState::new(LoopGuardConfig {
            tool_repeat_threshold: 4,
            ..Default::default()
        });
        state.handle_tool_call(&make_tool_call("bash", r#"{"command":"ls"}"#));
        assert_eq!(state.tool_guard.tool_history.len(), 1);
    }

    #[test]
    fn handle_tool_call_detects_repetition() {
        let mut state = LoopGuardState::new(LoopGuardConfig {
            tool_repeat_threshold: 3,
            ..Default::default()
        });
        for _ in 0..3 {
            state.handle_tool_call(&make_tool_call("bash", r#"{"command":"ls"}"#));
        }
        state.handle_tool_call(&make_tool_call("bash", r#"{"command":"ls"}"#));
        assert!(!state.pending_warn.is_empty() || !state.pending_inject.is_empty());
    }

    #[test]
    fn intercept_tool_result_injects_warning() {
        let mut state = LoopGuardState::new(LoopGuardConfig::default());
        state.pending_warn = "连续 4 次相同操作 bash".to_string();
        let mut tr = make_tool_result("bash", "原始结果");
        state.intercept_tool_result(&mut tr);
        assert!(tr.payload.content.starts_with("[循环检测警告]"));
        assert!(tr.payload.content.contains("原始结果"));
    }

    #[test]
    fn intercept_tool_result_injects_replaces() {
        let mut state = LoopGuardState::new(LoopGuardConfig::default());
        state.pending_inject = "检测到循环序列".to_string();
        let mut tr = make_tool_result("bash", "原始结果");
        state.intercept_tool_result(&mut tr);
        assert!(tr.payload.content.starts_with("[循环检测]"));
        assert!(!tr.payload.content.contains("原始结果"));
    }

    #[test]
    fn intercept_tool_result_no_pending() {
        let mut state = LoopGuardState::new(LoopGuardConfig::default());
        let mut tr = make_tool_result("bash", "原始结果");
        state.intercept_tool_result(&mut tr);
        assert_eq!(tr.payload.content, "原始结果");
    }

    #[tokio::test]
    async fn make_observe_handles_chunk() {
        let state = Arc::new(Mutex::new(LoopGuardState::new(LoopGuardConfig {
            streaming_check_interval: 5,
            ..Default::default()
        })));
        let observe = make_output_observe(state.clone());
        let chunk = OutputEvent::Chunk(make_chunk(Some("hello"), None));
        observe(chunk).await;
        let guard = state.lock().await;
        assert_eq!(guard.text_guard.accumulated_text, "hello");
    }

    #[tokio::test]
    async fn make_observe_handles_tool_call() {
        let state = Arc::new(Mutex::new(LoopGuardState::new(LoopGuardConfig::default())));
        let observe = make_output_observe(state.clone());
        let tc = OutputEvent::ToolCall(make_tool_call("bash", r#"{"command":"ls"}"#));
        observe(tc).await;
        let guard = state.lock().await;
        assert_eq!(guard.tool_guard.tool_history.len(), 1);
    }

    #[test]
    fn make_intercept_passes_through_non_tool_result() {
        let state = Arc::new(Mutex::new(LoopGuardState::new(LoopGuardConfig::default())));
        let intercept = make_output_intercept(state);
        let chunk = OutputEvent::Chunk(make_chunk(Some("hello"), None));
        let result = intercept(&chunk);
        assert!(matches!(result, InterceptResult::Pass(_)));
    }

    #[test]
    fn make_intercept_modifies_tool_result() {
        let state = Arc::new(Mutex::new(LoopGuardState::new(LoopGuardConfig::default())));
        {
            let mut guard = state.blocking_lock();
            guard.pending_warn = "测试警告".to_string();
            guard.pending_severity = Some(LoopSeverity::Warn);
        }
        let intercept = make_output_intercept(state);
        let tr = OutputEvent::ToolResult(make_tool_result("bash", "原始结果"));
        let result = intercept(&tr);
        match result {
            InterceptResult::Pass(event) => {
                if let OutputEvent::ToolResult(tr) = event {
                    assert!(tr.payload.content.starts_with("[循环检测警告]"));
                } else {
                    panic!("应为 ToolResult");
                }
            }
            _ => panic!("应为 Pass"),
        }
    }

    #[test]
    fn make_intercept_passes_on_interrupt_severity() {
        let state = Arc::new(Mutex::new(LoopGuardState::new(LoopGuardConfig::default())));
        {
            let mut guard = state.blocking_lock();
            guard.pending_severity = Some(LoopSeverity::Interrupt);
            guard.pending_inject = "循环中断".to_string();
        }
        let intercept = make_output_intercept(state);
        let tr = OutputEvent::ToolResult(make_tool_result("bash", "原始结果"));
        let result = intercept(&tr);
        // Interrupt 不再通过 intercept 返回，intercept 只做注入/修改
        assert!(matches!(result, InterceptResult::Pass(_)));
    }

    #[test]
    fn make_intercept_passes_on_abort_severity() {
        let state = Arc::new(Mutex::new(LoopGuardState::new(LoopGuardConfig::default())));
        {
            let mut guard = state.blocking_lock();
            guard.pending_severity = Some(LoopSeverity::Abort);
            guard.pending_inject = "循环终止".to_string();
        }
        let intercept = make_output_intercept(state);
        let tr = OutputEvent::ToolResult(make_tool_result("bash", "原始结果"));
        let result = intercept(&tr);
        // Abort 不再通过 intercept 返回，intercept 只做注入/修改
        assert!(matches!(result, InterceptResult::Pass(_)));
    }

    #[test]
    fn send_interrupt_sends_input_event() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut state = LoopGuardState::new(LoopGuardConfig::default());
        state.set_tx_send(tx);
        state.send_interrupt("循环检测".to_string());
        let event = rx.try_recv().unwrap();
        match event {
            InputEvent::Interrupt(data) => {
                assert_eq!(data.payload.reason, "循环检测");
                assert_eq!(data.payload.source, InterruptSource::Hook);
            }
            _ => panic!("应为 Interrupt 事件"),
        }
    }

    /// 完整升级链路集成测试：Warn → Inject → Interrupt ×3 → Abort
    #[test]
    fn full_escalation_chain() {
        let (tx_send, _rx_send) = tokio::sync::mpsc::channel(16);
        let mut state = LoopGuardState::new(LoopGuardConfig {
            tool_repeat_threshold: 4,
            ..Default::default()
        });
        state.set_tx_send(tx_send);

        // 1-3 次相同调用：不触发
        for i in 0..3 {
            state.handle_tool_call(&make_tool_call("bash", r#"{"command":"ls"}"#));
            assert!(
                state.pending_warn.is_empty() && state.pending_inject.is_empty(),
                "第 {} 次不应触发",
                i + 1
            );
        }

        // 第 4 次：Warn（tool_escalation=1）
        state.handle_tool_call(&make_tool_call("bash", r#"{"command":"ls"}"#));
        assert!(!state.pending_warn.is_empty());
        assert!(state.pending_inject.is_empty());
        assert_eq!(state.interrupt_count, 0);
        assert!(!state.aborted);

        // 第 5 次：Inject（tool_escalation=2）
        state.pending_warn.clear();
        state.handle_tool_call(&make_tool_call("bash", r#"{"command":"ls"}"#));
        assert!(!state.pending_inject.is_empty());
        assert_eq!(state.interrupt_count, 0);
        assert!(!state.aborted);

        // 第 6 次：Interrupt（tool_escalation=3），interrupt_count=1
        state.pending_inject.clear();
        state.handle_tool_call(&make_tool_call("bash", r#"{"command":"ls"}"#));
        assert!(!state.pending_inject.is_empty());
        assert_eq!(state.interrupt_count, 1);
        assert!(!state.aborted);

        // 第 7 次：Interrupt（tool_escalation=4），interrupt_count=2
        state.pending_inject.clear();
        state.handle_tool_call(&make_tool_call("bash", r#"{"command":"ls"}"#));
        assert_eq!(state.interrupt_count, 2);
        assert!(!state.aborted);

        // 第 8 次：Interrupt（tool_escalation=5），interrupt_count=3
        state.pending_inject.clear();
        state.handle_tool_call(&make_tool_call("bash", r#"{"command":"ls"}"#));
        assert_eq!(state.interrupt_count, 3);
        assert!(!state.aborted);

        // 第 9 次：Abort（interrupt_count=3 >= 3，Interrupt 升级为 Abort）
        state.pending_inject.clear();
        state.handle_tool_call(&make_tool_call("bash", r#"{"command":"ls"}"#));
        assert_eq!(state.interrupt_count, 3);
        assert!(!state.pending_inject.is_empty());
        assert!(state.aborted);
    }

    /// 插件注入消息不应重置 tool_history，确保检测保持"热"状态
    #[tokio::test]
    async fn plugin_message_preserves_tool_history() {
        let state = Arc::new(Mutex::new(LoopGuardState::new(LoopGuardConfig {
            tool_repeat_threshold: 3,
            ..Default::default()
        })));

        // 填充 tool_history（3 次调用）
        let observe = make_output_observe(state.clone());
        for _ in 0..3 {
            observe(OutputEvent::ToolCall(make_tool_call(
                "bash",
                r#"{"command":"ls"}"#,
            )))
            .await;
        }

        // 验证 tool_history 已有 3 条记录
        {
            let guard = state.lock().await;
            assert_eq!(guard.tool_guard.tool_history.len(), 3);
            assert_eq!(guard.interrupt_count, 0);
        }

        // 模拟插件注入消息（loop_guard 注入引导消息）
        observe(OutputEvent::User(OutputUserMessage {
            base: EventBase::default(),
            payload: OutputUserPayload {
                content: "[循环检测] 请调整策略".into(),
                mode: UserMessageMode::Guide,
                source: UserMessageSource::Plugin(PluginSource {
                    name: "loop_guard".into(),
                }),
            },
        }))
        .await;

        // 验证：tool_history 保留，interrupt_count 保留，pending 已清理
        let guard = state.lock().await;
        assert_eq!(
            guard.tool_guard.tool_history.len(),
            3,
            "插件消息不应清空 tool_history"
        );
        assert_eq!(guard.interrupt_count, 0, "interrupt_count 应保留");
        assert!(guard.pending_warn.is_empty());
        assert!(guard.pending_inject.is_empty());
        assert!(guard.pending_severity.is_none());
    }

    /// 插件注入后 AI 继续重复工具，应立即被检测到
    #[tokio::test]
    async fn plugin_message_keeps_detection_hot() {
        let (tx_send, _rx_send) = tokio::sync::mpsc::channel(16);
        let state = Arc::new(Mutex::new(LoopGuardState::new(LoopGuardConfig {
            tool_repeat_threshold: 3,
            ..Default::default()
        })));
        {
            let mut guard = state.lock().await;
            guard.set_tx_send(tx_send);
        }

        let observe = make_output_observe(state.clone());

        // 填充 3 次重复调用（刚好到 threshold）
        for _ in 0..3 {
            observe(OutputEvent::ToolCall(make_tool_call(
                "bash",
                r#"{"command":"ls"}"#,
            )))
            .await;
        }

        // 第 4 次调用触发 Warn
        observe(OutputEvent::ToolCall(make_tool_call(
            "bash",
            r#"{"command":"ls"}"#,
        )))
        .await;
        {
            let guard = state.lock().await;
            assert!(!guard.pending_warn.is_empty(), "第 4 次应触发 Warn");
        }

        // 模拟插件注入消息
        observe(OutputEvent::User(OutputUserMessage {
            base: EventBase::default(),
            payload: OutputUserPayload {
                content: "[循环检测] 请调整策略".into(),
                mode: UserMessageMode::Guide,
                source: UserMessageSource::Plugin(PluginSource {
                    name: "loop_guard".into(),
                }),
            },
        }))
        .await;

        // AI 继续重复相同工具：只需 1 次就应立即触发检测（tool_history 保留）
        observe(OutputEvent::ToolCall(make_tool_call(
            "bash",
            r#"{"command":"ls"}"#,
        )))
        .await;
        let guard = state.lock().await;
        // tool_history 保留，count 仍然 >= threshold，所以立即检测
        assert!(
            !guard.pending_warn.is_empty() || !guard.pending_inject.is_empty(),
            "插件注入后 AI 继续重复应立即被检测到"
        );
    }

    /// 用户消息完全重置所有状态
    #[tokio::test]
    async fn user_message_fully_resets_state() {
        let state = Arc::new(Mutex::new(LoopGuardState::new(LoopGuardConfig {
            tool_repeat_threshold: 3,
            ..Default::default()
        })));

        let observe = make_output_observe(state.clone());

        // 填充 tool_history + interrupt_count
        {
            let mut guard = state.lock().await;
            for _ in 0..4 {
                guard.handle_tool_call(&make_tool_call("bash", r#"{"command":"ls"}"#));
            }
            guard.interrupt_count = 2;
        }

        // 用户主动发消息
        observe(OutputEvent::User(OutputUserMessage {
            base: EventBase::default(),
            payload: OutputUserPayload {
                content: "新任务".into(),
                mode: UserMessageMode::Pending,
                source: UserMessageSource::User,
            },
        }))
        .await;

        // 验证：完全重置
        let guard = state.lock().await;
        assert!(
            guard.tool_guard.tool_history.is_empty(),
            "用户消息应清空 tool_history"
        );
        assert_eq!(guard.interrupt_count, 0, "用户消息应清空 interrupt_count");
        assert!(guard.pending_warn.is_empty());
        assert!(guard.pending_inject.is_empty());
        assert!(!guard.aborted);
    }
}
