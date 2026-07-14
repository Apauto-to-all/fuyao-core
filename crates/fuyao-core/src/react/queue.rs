//! 队列操作（双队列协同）
//!
//! guide / pending 两个对等队列的核心操作：
//! - [`consume_all_guide`]：一次性取出 guide 全部消息（非阻塞）
//! - [`drain_pending_to_guide`]：pending 全部倒进 guide（无条件，幂等）
//! - [`inject_messages`]：把一批队列消息注入 session.messages + 发 User 事件
//!
//! 消费语义（一次性全取）：触发消费时机时，guide 有多少条全部取出，
//! 每条对应一条 user message 全部 push 进 session.messages，回循环顶部调 LLM。

use crate::emit::Emitter;
use crate::engine::types::{QueuedUserMessage, SharedQueue};
use fuyao_api::message::EventBase;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::output::UserMessage as OutputUserMessage;
use fuyao_api::{Message, Session, UserMessageMode};

/// 一次性取出 guide 全部消息（非阻塞，drain 清空队列）
pub(crate) fn consume_all_guide(guide: &SharedQueue) -> Vec<QueuedUserMessage> {
    let mut q = guide.lock().unwrap_or_else(|e| e.into_inner());
    q.drain(..).collect()
}

/// pending 全部倒进 guide（无条件，幂等，锁顺序 pending→guide）
///
/// pending 为空时立即返回。锁顺序固定 pending 先、guide 后，
/// 与 Engine::send 的单锁 push 不冲突，无死锁风险。
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

/// 把一批队列消息注入 session.messages + 发 User 事件
///
/// 每条消息变一条 user message push 进历史，并发 OutputEvent::User 回显给 UI。
/// emitter 已含 session_id 标签（全程标签原则）。
pub(crate) async fn inject_messages(
    emitter: &Emitter,
    session: &mut Session,
    msgs: Vec<QueuedUserMessage>,
) {
    for m in msgs {
        session.messages.push(Message::user(m.content.clone()));
        emitter
            .emit(OutputEvent::User(OutputUserMessage {
                base: EventBase::default(),
                payload: fuyao_api::message::output::UserPayload {
                    content: m.content,
                    mode: UserMessageMode::Guide,
                    source: fuyao_api::UserMessageSource::User,
                },
            }))
            .await;
    }
}
