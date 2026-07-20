//! 队列操作（双队列协同）
//!
//! guide / pending 两个对等队列的核心操作：
//! - [`consume_all_guide`]：一次性取出 guide 全部消息（非阻塞）
//! - [`drain_pending_to_guide`]：pending 全部倒进 guide（无条件，幂等）
//! - [`inject_messages`]：把一批队列消息经 `emit_to_history` 单条落 DB
//!
//! 消费语义（一次性全取）：触发消费时机时，guide 有多少条全部取出，
//! 每条经 `emit_to_history`（拦截 → `store.insert_message` 落 DB → 发送事件 → 观察），
//! 与 assistant / tool_result 走完全相同的统一管道。
//!
//! User 消息的拦截/发送/观察**全在消费时刻**统一发生（入队纯排队，无 side effect）。

use super::SessionCtx;
use crate::dispatch;
use crate::engine::types::SharedQueue;
use fuyao_api::InboundUser;
use fuyao_api::{Message, OutputEvent, Session};

/// 一次性取出 guide 全部消息（非阻塞，drain 清空队列）
pub(crate) fn consume_all_guide(guide: &SharedQueue) -> Vec<InboundUser> {
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

/// 把一批队列消息经 `emit_to_history` 单条落 DB
///
/// 每条 `InboundUser` 的 `message`（output 侧 UserMessage）取出包成
/// `OutputEvent::User`，经统一管道：拦截 → 构造 `Message::user` 调
/// `store.insert_message` 单条落 DB → 发送事件给 UI → 观察钩子。
///
/// 与 assistant / tool_result 完全对称——拦截/存储/发送三者同源，插件可在消费时刻
/// 改写或阻断 user 消息（修复"拦截裂缝在 user 消息上重现"的结构性缺陷）。
///
/// Block 时：该消息不落库、不发（插件的责任，与 assistant Block 语义一致）。
pub(crate) async fn inject_messages(
    ctx: &SessionCtx,
    session: &mut Session,
    msgs: Vec<InboundUser>,
) {
    for m in msgs {
        let event = OutputEvent::User(m.message);
        let _ = dispatch::emit_to_history(
            &ctx.emitter,
            &ctx.hooks,
            ctx.store.as_ref(),
            session,
            event,
            user_msg_from_event,
        )
        .await;
    }
}

/// 从 User 输出事件构造 `Message::user`（emit_to_history 闭包）
///
/// 拦截后的 content 用于构造 Message——保证「拦截 → 存储 → 发送」三者一致。
fn user_msg_from_event(ev: &OutputEvent) -> Option<Message> {
    match ev {
        OutputEvent::User(m) => Some(Message::user(m.payload.content.clone())),
        _ => None,
    }
}
