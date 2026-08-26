//! 队列操作（双队列协同）
//!
//! guide / pending 两个对等队列的核心操作：
//! - [`consume_all_guide`]：一次性取出 guide 全部条目（非阻塞）
//! - [`drain_pending_to_guide`]：pending 全部倒进 guide（无条件，幂等）
//!
//! 消费语义（一次性全取）：触发消费时机时，guide 有多少条全部取出。
//! 取出的 `QueueEntry` 由消费点统一处理：连续 User 段批量经统一历史入口
//! （`crate::history::inject_user_messages`：拦截 → 落 DB → 发送事件 → 观察），
//! Control 条目就地执行命令本体。
//!
//! User 消息的拦截/发送/观察**全在消费时刻**统一发生（入队纯排队，无 side effect）。

use crate::engine::types::{QueueEntry, SharedQueue};

/// 一次性取出 guide 全部条目（非阻塞，drain 清空队列）
pub(crate) fn consume_all_guide(guide: &SharedQueue) -> Vec<QueueEntry> {
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
