//! Session 级消息发送器
//!
//! [`SessionSender`] 封装"往**某个 session** 发消息"的能力，三类消息分流到
//! 各自通道。插件在 session 装配时经
//! [`PluginInstance::register`](crate::PluginInstance::register) 直接拿到 SessionSender，
//! 保存到自己的 state 里随时调用。
//!
//! 三类消息的分流：
//! - `User` → session 统一入站通道（与外部用户消息同通道同类型，发送顺序即排队顺序；
//!   消费时机过完整管道：拦截→处理→发送→观察）
//! - `Interrupt` → session 中断通道（select! 中断点监听，打断当前 ReAct）
//! - `Notice` → per-session 出站通道直送（不经 ReAct 循环、不经拦截/观察面、不落库）
//!
//! 插件自主决定何时发送，引擎只负责消费。
//! 所有发送方法用非阻塞 send（try_send / 无界 send），失败记 warn（不阻塞 hook 执行）。

use fuyao_api::InterruptSource;
use fuyao_api::message::EventBase;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::QueueEntry;
use fuyao_api::message::input::{PluginSource, UserMessageMode, UserMessageSource};
use fuyao_api::message::output::InterruptMessage as OutputInterruptMessage;
use fuyao_api::message::output::NoticeLevel;
use fuyao_api::message::output::PluginNoticeMessage as OutputPluginNoticeMessage;
use fuyao_api::message::output::UserMessage as OutputUserMessage;
use fuyao_api::message::output::UserPayload as OutputUserPayload;
use tokio::sync::mpsc::{Sender, UnboundedSender};

/// Session 级消息发送器
///
/// 绑定一个插件名（自动填充注入消息的 source 字段），持有该 session 各通道的
/// sender。引擎在每个 session 装配时按插件构造一份（绑该插件名 + session_id +
/// 该 session 的通道），经 [`PluginInstance::register`](crate::PluginInstance::register)
/// 传给插件。插件 clone 后保存（[`Clone`] 已实现）。
///
/// 所有方法用非阻塞 send：有界通道 `try_send`（满或关闭仅记 warn），
/// 无界通道 `send`（仅关闭记 warn）——不阻塞 hook 执行。
///
/// 通道载荷统一为 output 侧类型——插件是内核内组件，直接产出 output 侧消息，
/// 不经 input 中间态（与外部 `InputEvent` 经 `Engine::send` 入口转化的路径在地基上统一）。
#[derive(Clone)]
pub struct SessionSender {
    /// 绑定的插件名（自动填注入消息的 source 字段）
    name: String,
    /// 绑定的 session id（Notice 事件直送出站通道时自盖标签）
    session_id: String,
    /// 统一入站通道发送端（User 条目排队，与外部入站同通道）
    tx_inbound: Sender<QueueEntry>,
    /// Interrupt 消息发送端（送进 session 中断通道）
    tx_interrupt: Sender<OutputInterruptMessage>,
    /// Notice 事件发送端（per-session 出站通道，直达消费者）
    tx_event: UnboundedSender<OutputEvent>,
}

impl SessionSender {
    /// 构造（引擎在 session 装配时调用，传入插件名 + session_id + 该 session的通道 sender）
    pub fn new(
        name: impl Into<String>,
        session_id: impl Into<String>,
        tx_inbound: Sender<QueueEntry>,
        tx_interrupt: Sender<OutputInterruptMessage>,
        tx_event: UnboundedSender<OutputEvent>,
    ) -> Self {
        Self {
            name: name.into(),
            session_id: session_id.into(),
            tx_inbound,
            tx_interrupt,
            tx_event,
        }
    }

    /// 绑定的插件名（只读，调试/日志用）
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 发送 User 消息（指定模式）
    ///
    /// `mode` 决定消息入哪个队列：
    /// - `Guide`：进入引导队列，AI 完成一轮（如工具调用）后立即投递
    /// - `Pending`：进入排队队列，AI 不再调工具（最终回复）后才投递
    ///
    /// source 字段自动标记为 `Plugin`，携带本插件的名称——保证来源可追溯。
    pub fn send_user(&self, content: impl Into<String>, mode: UserMessageMode) {
        let result = self
            .tx_inbound
            .try_send(QueueEntry::User(OutputUserMessage {
                base: EventBase::default(),
                payload: OutputUserPayload {
                    content: content.into(),
                    images: vec![],
                    mode,
                    source: UserMessageSource::Plugin(PluginSource {
                        name: self.name.clone(),
                    }),

                    client_message_id: None,
                },
            }));
        if let Err(e) = result {
            tracing::warn!(
                plugin = %self.name,
                channel = "inbound",
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

    /// 发送插件通知
    ///
    /// 通知直达 per-session 出站通道，**不经 dispatch 管道**——三重刻意设计：
    /// - 不经 intercept：通知不可被其他插件 Block（拦截权用于内容管制，
    ///   不用于静音别的插件）
    /// - 不经 observe：插件观察钩子看到通知再回通知会形成反馈环，
    ///   通知不属于插件扩展面，只属于消费者（CLI/TUI）
    /// - 不落库：纯实时事件（seq 恒 None），不进聊天历史
    ///
    /// source 自动填本插件名（可追溯）；session_id 由本方法盖标签
    /// （与 Emitter::emit 的「session id 全程标签」原则同一约定）。
    /// 出站通道无界，send 永不阻塞——钩子体内调用安全；通道关闭仅记 warn。
    pub fn send_notice(&self, content: impl Into<String>, level: NoticeLevel) {
        let mut event = OutputEvent::PluginNotice(OutputPluginNoticeMessage::new(
            self.name.clone(),
            level,
            content,
        ));
        event.base_mut().session_id = Some(self.session_id.clone());
        if self.tx_event.send(event).is_err() {
            tracing::warn!(
                plugin = %self.name,
                channel = "notice",
                "插件发送 Notice 事件失败（出站通道已关闭）"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::make_sender;

    /// send_notice：事件到达出站通道，session_id 已盖标签，source 填插件名，级别透传
    #[tokio::test]
    async fn send_notice_stamps_session_id_and_source() {
        let (sender, _rx_inbound, _rx_interrupt, mut rx_event) =
            make_sender("test_plugin", "sess-1");
        sender.send_notice("检测到循环", NoticeLevel::Info);
        let event = rx_event.recv().await.expect("应收到通知事件");
        let OutputEvent::PluginNotice(m) = event else {
            panic!("应是 PluginNotice 事件");
        };
        assert_eq!(m.base.session_id.as_deref(), Some("sess-1"));
        assert_eq!(m.payload.source.name, "test_plugin");
        assert_eq!(m.payload.level, NoticeLevel::Info);
        assert_eq!(m.payload.content, "检测到循环");
    }

    /// send_notice：Error 级别透传
    #[tokio::test]
    async fn send_notice_passes_level() {
        let (sender, _rx_inbound, _rx_interrupt, mut rx_event) =
            make_sender("test_plugin", "sess-1");
        sender.send_notice("已终止", NoticeLevel::Error);
        let OutputEvent::PluginNotice(m) = rx_event.recv().await.expect("应收到通知事件")
        else {
            panic!("应是 PluginNotice 事件");
        };
        assert_eq!(m.payload.level, NoticeLevel::Error);
        assert_eq!(m.payload.content, "已终止");
    }
}
