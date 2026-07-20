//! Session 级消息发送器
//!
//! [`SessionSender`] 封装"往**某个 session** 发消息"的能力，三种消息类型分流到
//! session 级三条独立通道。插件通过 [`send_input`](crate::SendInputFn) hook 在
//! session 装配时拿到 SessionSender，之后保存到自己的 state 里随时调用。
//!
//! 三种消息类型的分流：
//! - `User` → session 入站通道（送进 session task 过完整管道：拦截→处理→发送→观察）
//! - `Interrupt` → session 中断通道（select! 中断点监听，打断当前 ReAct）
//! - `Plugin` → session Plugin 通道（送进 session task 过 dispatch 管道发外部通知）
//!
//! 这是"真·主动"模式：插件自主决定何时发送，引擎只负责消费。
//! 所有发送方法用 `try_send`（非阻塞），失败记 warn（不阻塞 hook 执行）。

use fuyao_api::InboundUser;
use fuyao_api::MessageParams;
use fuyao_api::message::EventBase;
use fuyao_api::message::input::{
    InterruptMessage, InterruptPayload, InterruptSource, PluginEventSource, PluginMessage,
    PluginPayload, PluginSource, UserMessageMode, UserMessageSource,
};
use fuyao_api::message::output::UserMessage as OutputUserMessage;
use fuyao_api::message::output::UserPayload as OutputUserPayload;
use tokio::sync::mpsc::Sender;

/// Session 级消息发送器
///
/// 绑定一个插件身份（自动填充 Plugin 消息的 source 字段），持有三条通道的 sender。
/// 引擎在每个 session 装配时构造一份（绑该 session 的通道），通过 send_input hook
/// 传给插件。插件 clone 后保存（[`Clone`] 已实现）。
///
/// 所有方法用 `try_send`（非阻塞）：通道满或关闭时静默忽略，仅记 warn 日志。
#[derive(Clone)]
pub struct SessionSender {
    /// 绑定的插件身份（自动填 Plugin 消息的 source 字段）
    identity: PluginEventSource,
    /// User 消息发送端（送进 session 入站通道）
    tx_user: Sender<InboundUser>,
    /// Interrupt 消息发送端（送进 session 中断通道）
    tx_interrupt: Sender<InterruptMessage>,
    /// Plugin 消息发送端（送进 session Plugin 通道）
    tx_plugin: Sender<PluginMessage>,
}

impl SessionSender {
    /// 构造（引擎在 session 装配时调用，传入该 session 的三条通道 sender）
    pub fn new(
        identity: PluginEventSource,
        tx_user: Sender<InboundUser>,
        tx_interrupt: Sender<InterruptMessage>,
        tx_plugin: Sender<PluginMessage>,
    ) -> Self {
        Self {
            identity,
            tx_user,
            tx_interrupt,
            tx_plugin,
        }
    }

    /// 绑定的身份（只读，调试/日志用）
    pub fn identity(&self) -> &PluginEventSource {
        &self.identity
    }

    /// 发送 User 消息（默认 Guide 模式，触发 ReAct 循环）
    ///
    /// 等价于 [`send_user_with_mode`](Self::send_user_with_mode)`(content, UserMessageMode::Guide)`。
    pub fn send_user(&self, content: impl Into<String>) {
        self.send_user_with_mode(content, UserMessageMode::Guide);
    }

    /// 发送 User 消息（指定模式）
    ///
    /// `mode` 决定消息入哪个队列：
    /// - `Guide`：进入引导队列，AI 完成一轮（如工具调用）后立即投递
    /// - `Pending`：进入排队队列，AI 不再调工具（最终回复）后才投递
    ///
    /// source 字段自动标记为 `Plugin`，携带本插件的 identity 名称——保证来源可追溯。
    pub fn send_user_with_mode(&self, content: impl Into<String>, mode: UserMessageMode) {
        let result = self.tx_user.try_send(InboundUser {
            message: OutputUserMessage {
                base: EventBase::default(),
                payload: OutputUserPayload {
                    content: content.into(),
                    mode,
                    source: UserMessageSource::Plugin(PluginSource {
                        name: self.identity.name.clone(),
                    }),
                },
            },
            params: MessageParams::default(),
        });
        if let Err(e) = result {
            tracing::warn!(
                plugin = %self.identity.name,
                channel = "user",
                cause = %e,
                "插件发送 User 消息失败"
            );
        }
    }

    /// 发送 Interrupt 消息（中断当前 session 的执行）
    ///
    /// source 自动标记为 `Hook`，与用户主动中断区分。
    pub fn send_interrupt(&self, reason: impl Into<String>) {
        let result = self.tx_interrupt.try_send(InterruptMessage {
            base: EventBase::default(),
            payload: InterruptPayload {
                reason: reason.into(),
                source: InterruptSource::Hook,
            },
        });
        if let Err(e) = result {
            tracing::warn!(
                plugin = %self.identity.name,
                channel = "interrupt",
                cause = %e,
                "插件发送 Interrupt 消息失败"
            );
        }
    }

    /// 发送 Plugin 消息（最简形式：只有 message 字符串）
    ///
    /// 用于最常见的"插件通知 UI"场景（如循环检测的警告文本）。
    pub fn send_plugin(&self, event_type: &str, message: impl Into<String>) {
        self.send_plugin_full(event_type, None, None, Some(message.into()));
    }

    /// 发送 Plugin 消息（带 data 字段，用于统计/进度等结构化数据）
    pub fn send_plugin_data(&self, event_type: &str, data: serde_json::Value) {
        self.send_plugin_full(event_type, Some(data), None, None);
    }

    /// 发送完整 Plugin 消息（自动填 source = identity, base = default）
    ///
    /// source 字段自动绑定构造时的 identity，无需调用方手填，杜绝命名漂移。
    pub fn send_plugin_full(
        &self,
        event_type: &str,
        data: Option<serde_json::Value>,
        error: Option<String>,
        message: Option<String>,
    ) {
        let result = self.tx_plugin.try_send(PluginMessage {
            base: EventBase::default(),
            payload: PluginPayload {
                source: self.identity.clone(),
                event_type: event_type.to_string(),
                data,
                error,
                message,
            },
        });
        if let Err(e) = result {
            tracing::warn!(
                plugin = %self.identity.name,
                channel = "plugin",
                cause = %e,
                "插件发送 Plugin 消息失败"
            );
        }
    }
}
