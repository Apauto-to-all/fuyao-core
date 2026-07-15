//! 队列操作（双队列协同）
//!
//! guide / pending 两个对等队列的核心操作：
//! - [`consume_all_guide`]：一次性取出 guide 全部消息（非阻塞）
//! - [`drain_pending_to_guide`]：pending 全部倒进 guide（无条件，幂等）
//! - [`inject_messages`]：把一批队列消息注入 session.messages（只推进历史，不发回显）
//!
//! 消费语义（一次性全取）：触发消费时机时，guide 有多少条全部取出，
//! 每条对应一条 user message 全部 push 进 session.messages，回循环顶部调 LLM。
//!
//! User 消息的回显（OutputEvent::User）在入站时已过管道发出（见 react/mod.rs 的
//! handle_inbound_user），此处 inject 只负责推进历史，不再发回显。

use crate::engine::types::{QueuedUserMessage, SharedQueue};
use fuyao_api::{Message, Session};

/// 一次性取出 guide 全部消息（非阻塞，drain 清空队列）
pub(crate) fn consume_all_guide(guide: &SharedQueue) -> Vec<QueuedUserMessage> {
    let mut q = guide.lock().unwrap_or_else(|e| e.into_inner());
    q.drain(..).collect()
}

/// pending 全部倒进 guide（无条件，幂等，锁顺序 pending→guide）
///
/// pending 为空时立即返回。锁顺序固定 pending 先、guide 后，无死锁风险。
pub(crate) fn drain_pending_to_guide(guide: &SharedQueue, pending: &SharedQueue) {
    let mut p = pending.lock().unwrap_or_else(|e| e.into_inner());
    if p.is_empty() {
        return;
    }
    let mut g = guide.lock().unwrap_or_else(|e| e.into_inner());
    while let Some(q) = p.pop_front() {
        g.push_back(q);
    }
}

/// 把一批队列消息注入 session.messages（只推进历史，不发回显）
///
/// 每条消息变一条 user message push 进历史。
/// User 回显事件在入站管道已发（handle_inbound_user），此处只推进 session.messages。
pub(crate) fn inject_messages(session: &mut Session, msgs: Vec<QueuedUserMessage>) {
    for m in msgs {
        session.messages.push(Message::user(m.content));
    }
}
