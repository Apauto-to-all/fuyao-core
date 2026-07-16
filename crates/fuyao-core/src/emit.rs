//! 事件发射辅助
//!
//! 引擎内部发出 OutputEvent 的统一入口。
//! 核心职责：给事件的 base 盖上 session_id 标签（全程标签原则），
//! 然后发到出口通道。

use fuyao_api::message::OutputEvent;
use tokio::sync::mpsc::Sender;

/// 事件发射器（session 级，聚合出口通道 + session_id）
///
/// 把 `tx_event` + `session_id` 打包成 owned 结构体，避免在每个调用点
/// 重复传这两个参数。所有 task 内部发事件都经它——保证 session_id 标签不丢。
#[derive(Clone)]
pub(crate) struct Emitter {
    tx: Sender<OutputEvent>,
    session_id: String,
}

impl Emitter {
    pub fn new(tx: Sender<OutputEvent>, session_id: String) -> Self {
        Self { tx, session_id }
    }

    /// 发出一个事件，盖上 session_id 标签后送入出口通道
    ///
    /// 设计文档「session id 全程标签」原则：事件一产生就带编号，
    /// 消费者拿任意一条事件都能取到 session_id 分流。
    pub async fn emit(&self, event: OutputEvent) {
        let mut event = event;
        stamp_session_id(&mut event, &self.session_id);
        if self.tx.send(event).await.is_err() {
            tracing::warn!(session_id = %self.session_id, "事件出口通道已关闭，事件丢弃");
        }
    }

    /// session id 引用（日志等场景用）
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

/// 给事件的 base.session_id 盖标签（递归处理所有带 base 的变体）
fn stamp_session_id(event: &mut OutputEvent, session_id: &str) {
    let id = Some(session_id.to_string());
    match event {
        OutputEvent::Chunk(m) => m.base.session_id = id,
        OutputEvent::User(m) => m.base.session_id = id,
        OutputEvent::ToolCall(m) => m.base.session_id = id,
        OutputEvent::ToolResult(m) => m.base.session_id = id,
        OutputEvent::Assistant(m) => m.base.session_id = id,
        OutputEvent::Interrupt(m) => m.base.session_id = id,
        OutputEvent::Error(m) => m.base.session_id = id,
        OutputEvent::Plugin(m) => m.base.session_id = id,
    }
}
