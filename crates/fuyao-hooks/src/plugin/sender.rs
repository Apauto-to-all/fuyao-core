//! Session 级消息发送器
//!
//! [`SessionSender`] 封装"往**某个 session** 发消息"的能力，两种消息类型分流到
//! session 级两条既有通道。插件在 session 装配时经
//! [`PluginInstance::register`](crate::PluginInstance::register) 直接拿到 SessionSender，
//! 保存到自己的 state 里随时调用。
//!
//! 两种消息类型的分流：
//! - `User` → session 入站通道（送进 session task 过完整管道：拦截→处理→发送→观察）
//! - `Interrupt` → session 中断通道（select! 中断点监听，打断当前 ReAct）
//!
//! 插件自主决定何时发送，引擎只负责消费。
//! 所有发送方法用 `try_send`（非阻塞），失败记 warn（不阻塞 hook 执行）。

use fuyao_api::InterruptSource;
use fuyao_api::message::EventBase;
use fuyao_api::message::input::{PluginSource, UserMessageMode, UserMessageSource};
use fuyao_api::message::output::InterruptMessage as OutputInterruptMessage;
use fuyao_api::message::output::UserMessage as OutputUserMessage;
use fuyao_api::message::output::UserPayload as OutputUserPayload;
use tokio::sync::mpsc::Sender;

/// Session 级消息发送器
///
/// 绑定一个插件名（自动填充注入消息的 source 字段），持有两条通道的 sender。
/// 引擎在每个 session 装配时按插件构造一份（绑该插件名 + 该 session 的通道），
/// 经 [`PluginInstance::register`](crate::PluginInstance::register) 传给插件。
/// 插件 clone 后保存（[`Clone`] 已实现）。
///
/// 所有方法用 `try_send`（非阻塞）：通道满或关闭时静默忽略，仅记 warn 日志。
///
/// 两条通道载荷统一为 output 侧类型——插件是内核内组件，直接产出 output 侧消息，
/// 不经 input 中间态（与外部 `InputEvent` 经 `Engine::send` 入口转化的路径在地基上统一）。
#[derive(Clone)]
pub struct SessionSender {
    /// 绑定的插件名（自动填注入消息的 source 字段）
    name: String,
    /// User 消息发送端（送进 session 入站通道）
    tx_user: Sender<OutputUserMessage>,
    /// Interrupt 消息发送端（送进 session 中断通道）
    tx_interrupt: Sender<OutputInterruptMessage>,
}

impl SessionSender {
    /// 构造（引擎在 session 装配时调用，传入插件名 + 该 session 的两条通道 sender）
    pub fn new(
        name: impl Into<String>,
        tx_user: Sender<OutputUserMessage>,
        tx_interrupt: Sender<OutputInterruptMessage>,
    ) -> Self {
        Self {
            name: name.into(),
            tx_user,
            tx_interrupt,
        }
    }

    /// 绑定的插件名（只读，调试/日志用）
    pub fn name(&self) -> &str {
        &self.name
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
    /// source 字段自动标记为 `Plugin`，携带本插件的名称——保证来源可追溯。
    pub fn send_user_with_mode(&self, content: impl Into<String>, mode: UserMessageMode) {
        let result = self.tx_user.try_send(OutputUserMessage {
            base: EventBase::default(),
            payload: OutputUserPayload {
                content: content.into(),
                images: vec![],
                mode,
                source: UserMessageSource::Plugin(PluginSource {
                    name: self.name.clone(),
                }),
            },
        });
        if let Err(e) = result {
            tracing::warn!(
                plugin = %self.name,
                channel = "user",
                cause = %e,
                "插件发送 User 消息失败"
            );
        }
    }

    /// 发送 Interrupt 消息（中断当前 session 的执行）
    ///
    /// source 自动标记为 `Hook`，与用户主动中断区分。
    /// 直接产出 output 侧 InterruptMessage（内核内组件不经 input 中间态）。
    pub fn send_interrupt(&self, reason: impl Into<String>) {
        let result = self
            .tx_interrupt
            .try_send(OutputInterruptMessage::new(reason, InterruptSource::Hook));
        if let Err(e) = result {
            tracing::warn!(
                plugin = %self.name,
                channel = "interrupt",
                cause = %e,
                "插件发送 Interrupt 消息失败"
            );
        }
    }
}
