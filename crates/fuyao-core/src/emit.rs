//! 事件发射辅助
//!
//! 引擎内部发出 OutputEvent 的统一入口。
//! 核心职责：给事件的 base 盖上 session_id 标签（全程标签原则），
//! 然后发到出口通道。

use fuyao_api::message::OutputEvent;
use tokio::sync::mpsc::Sender;

/// 发出一个事件，盖上 session_id 标签后送入出口通道
///
/// 设计文档「session id 全程标签」原则：事件一产生就带编号，
/// 消费者拿任意一条事件都能取到 session_id 分流。
/// 此函数覆盖事件的 base.session_id（decoder 产出的 Chunk 等事件 base.session_id 是 None）。
pub(crate) async fn emit_event(
    tx_event: &Sender<OutputEvent>,
    session_id: &str,
    mut event: OutputEvent,
) {
    stamp_session_id(&mut event, session_id);
    if tx_event.send(event).await.is_err() {
        tracing::warn!(session_id = session_id, "事件出口通道已关闭，事件丢弃");
    }
}

/// 给事件的 base.session_id 盖标签（递归处理所有带 base 的变体）
fn stamp_session_id(event: &mut OutputEvent, session_id: &str) {
    let id = Some(session_id.to_string());
    match event {
        OutputEvent::TurnStart(m) => m.base.session_id = id,
        OutputEvent::Chunk(m) => m.base.session_id = id,
        OutputEvent::User(m) => m.base.session_id = id,
        OutputEvent::ToolCall(m) => m.base.session_id = id,
        OutputEvent::ToolResult(m) => m.base.session_id = id,
        OutputEvent::Assistant(m) => m.base.session_id = id,
        OutputEvent::Interrupt(m) => m.base.session_id = id,
        OutputEvent::Error(m) => m.base.session_id = id,
        OutputEvent::Plugin(m) => m.base.session_id = id,
        OutputEvent::QueueUpdate(m) => m.base.session_id = id,
    }
}
