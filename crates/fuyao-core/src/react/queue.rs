//! 队列操作与消费时机（双队列协同）
//!
//! guide / pending 两个对等队列的**消费时机唯一权威定义**在本模块：
//! - 原语操作：[`consume_all_guide`]（一次性取出 guide 全部条目）与
//!   [`drain_pending_to_guide`]（pending 全部倒进 guide，无条件幂等）
//! - [`ConsumeTiming`]：三个消费点各自的相位标识与取队规则
//! - [`ConsumeGate`]：消费许可（跨 turn 状态）+ 相位取件的统一查询入口
//!
//! 消费语义（一次性全取）：触发消费时机时，guide 有多少条全部取出。
//! 取出的 `QueueEntry` 由消费点统一处理：连续 User 段批量经统一历史入口
//! （`crate::history::inject_user_messages`：拦截 → 落 DB → 发送事件 → 观察），
//! Control 条目就地执行命令本体。
//!
//! User 消息的拦截/发送/观察**全在消费时刻**统一发生（入队纯排队，无 side effect）。

use super::turn::TurnOutcome;
use crate::engine::types::SharedQueue;
use fuyao_api::message::QueueEntry;

/// 一次性取出 guide 全部条目（非阻塞，drain 清空队列）
fn consume_all_guide(guide: &SharedQueue) -> Vec<QueueEntry> {
    let mut q = guide.lock().unwrap_or_else(|e| e.into_inner());
    q.drain(..).collect()
}

/// pending 全部倒进 guide（无条件，幂等，锁顺序 pending→guide）
///
/// pending 为空时立即返回。锁顺序固定 pending 先、guide 后，无死锁风险。
fn drain_pending_to_guide(guide: &SharedQueue, pending: &SharedQueue) {
    let mut p = pending.lock().unwrap_or_else(|e| e.into_inner());
    if p.is_empty() {
        return;
    }
    let mut g = guide.lock().unwrap_or_else(|e| e.into_inner());
    while let Some(q) = p.pop_front() {
        g.push_back(q);
    }
}

/// 消费时机：三个消费点的相位标识，各变体内定该相位下 guide / pending 的取队规则
///
/// - **工具批完成后**：只解禁 guide——guide 即投递，pending 保持锁定
///   （ReAct 链还在进行，「等链结束」条件未满足）
/// - **最终回复后**：pending 先倒灌 guide（追加在 guide 现有内容之后），
///   合并后一次性全取
/// - **空闲**：无活跃 turn，pending 的「等链结束」解禁条件已天然满足——
///   guide 优先消费，guide 空时 pending 倒灌解禁（否则只发 pending 会死信）
#[derive(Debug, Clone, Copy)]
pub(crate) enum ConsumeTiming {
    /// 一批工具全部执行完成后、发回 AI 前
    AfterToolBatch,
    /// AI 给出最终回复（不再调用工具）、一轮 ReAct 结束后
    FinalReply,
    /// task 空闲（主循环顶部，无活跃 turn）
    Idle,
}

/// 双队列消费门——「guide / pending 何时可消费」的唯一权威定义
///
/// 两层规则内化为类型行为，全部判定与迁移经本类型的方法发生：
///
/// # 消费许可（跨 turn 状态）
///
/// - turn 以 [`TurnOutcome::Completed`] 结束（双队列跑空、AI 给最终回复）→
///   许可开放，主循环继续取件或落 select! 等待
/// - 中断 / 失败退出 → 许可暂停，guide / pending 剩余**原样保留**（引擎不清队列），
///   任何相位取件一律空取，主循环自然落 select! 等待
/// - 新条目入队（用户消息 / 控制命令 / 插件注入）= 新意图 →
///   [`resume_on_new_intent`](Self::resume_on_new_intent) 恢复许可，
///   主循环回顶部把「旧剩余 + 新条目」一起消费（忠实消费）
///
/// # 相位取件
///
/// 三个消费点（主循环顶 / 工具批完成后 / 最终回复后）统一经
/// [`take`](Self::take) 查询取件，各相位的取队规则见 [`ConsumeTiming`] 变体文档。
///
/// 修改消费语义只需动本模块。
#[derive(Debug, Default)]
pub(crate) struct ConsumeGate {
    /// 暂停消费：上次 turn 中断 / 失败后为真；默认（引擎启动）为假 = 许可开放
    paused: bool,
}

impl ConsumeGate {
    /// turn 结束：按退出原因迁移许可
    ///
    /// `Completed` 开放许可；中断 / 失败暂停许可（队列剩余保留，等新条目恢复）。
    pub(crate) fn on_turn_end(&mut self, outcome: TurnOutcome) {
        self.paused = !matches!(outcome, TurnOutcome::Completed);
    }

    /// 新条目入队：恢复消费许可
    ///
    /// 新条目 = 新意图，清除暂停（含命令条目——否则中断后 idle 发的命令会死信），
    /// 由主循环 idle select! 的 inbound 分支调用。
    pub(crate) fn resume_on_new_intent(&mut self) {
        self.paused = false;
    }

    /// 相位取件：在指定时机取出应消费的队列条目
    ///
    /// 暂停态一律空取（不触碰队列，剩余原样保留）；许可开放时按
    /// [`ConsumeTiming`] 的相位规则取件。
    pub(crate) fn take(
        &self,
        timing: ConsumeTiming,
        guide: &SharedQueue,
        pending: &SharedQueue,
    ) -> Vec<QueueEntry> {
        if self.paused {
            return Vec::new();
        }
        match timing {
            ConsumeTiming::AfterToolBatch => consume_all_guide(guide),
            ConsumeTiming::FinalReply => {
                drain_pending_to_guide(guide, pending);
                consume_all_guide(guide)
            }
            ConsumeTiming::Idle => {
                let entries = consume_all_guide(guide);
                if entries.is_empty() {
                    drain_pending_to_guide(guide, pending);
                    consume_all_guide(guide)
                } else {
                    entries
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::UserMessageMode;
    use fuyao_api::message::EventBase;
    use fuyao_api::message::input::UserMessageSource;
    use fuyao_api::message::output::{UserMessage, UserPayload};
    use std::sync::Arc;

    /// 构造带内容标识的 User 条目（断言用 content 区分条目）
    fn user_entry(content: &str) -> QueueEntry {
        QueueEntry::User(UserMessage {
            base: EventBase::default(),
            payload: UserPayload {
                content: content.to_string(),
                images: vec![],
                mode: UserMessageMode::Guide,
                source: UserMessageSource::User,
                client_message_id: None,
            },
        })
    }

    /// 构造已预排条目的队列
    fn queue_with(entries: Vec<QueueEntry>) -> SharedQueue {
        let q: SharedQueue = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        {
            let mut guard = q.lock().unwrap();
            for e in entries {
                guard.push_back(e);
            }
        }
        q
    }

    /// 取条目列表的内容标识（按队列顺序）
    fn contents(entries: &[QueueEntry]) -> Vec<String> {
        entries
            .iter()
            .filter_map(|e| match e {
                QueueEntry::User(m) => Some(m.payload.content.clone()),
                QueueEntry::Control(_) => None,
            })
            .collect()
    }

    /// 取队列当前残留的内容标识（按队列顺序）
    fn queue_contents(q: &SharedQueue) -> Vec<String> {
        let guard = q.lock().unwrap();
        guard
            .iter()
            .filter_map(|e| match e {
                QueueEntry::User(m) => Some(m.payload.content.clone()),
                QueueEntry::Control(_) => None,
            })
            .collect()
    }

    /// 时机①：工具批完成后只解禁 guide——pending 保持锁定，原样留在队列
    #[test]
    fn after_tool_batch_takes_guide_and_locks_pending() {
        let gate = ConsumeGate::default();
        let guide = queue_with(vec![user_entry("g1"), user_entry("g2")]);
        let pending = queue_with(vec![user_entry("p1")]);

        let taken = gate.take(ConsumeTiming::AfterToolBatch, &guide, &pending);

        assert_eq!(contents(&taken), vec!["g1", "g2"]);
        assert!(guide.lock().unwrap().is_empty(), "guide 应被消费空");
        assert_eq!(
            queue_contents(&pending),
            vec!["p1"],
            "时机①不动 pending（等链结束条件未满足）"
        );
    }

    /// 时机②：最终回复后 pending 倒灌 guide（追加在 guide 现有内容之后）再全取
    #[test]
    fn final_reply_pours_pending_after_guide_then_takes_all() {
        let gate = ConsumeGate::default();
        let guide = queue_with(vec![user_entry("g1")]);
        let pending = queue_with(vec![user_entry("p1"), user_entry("p2")]);

        let taken = gate.take(ConsumeTiming::FinalReply, &guide, &pending);

        assert_eq!(
            contents(&taken),
            vec!["g1", "p1", "p2"],
            "倒灌顺序：guide 现有内容在前、pending 追加在后"
        );
        assert!(guide.lock().unwrap().is_empty());
        assert!(pending.lock().unwrap().is_empty());
    }

    /// 空闲相位：guide 优先消费（非空时 pending 不动）；guide 空时 pending 倒灌解禁
    #[test]
    fn idle_prefers_guide_and_unlocks_pending_only_when_guide_empty() {
        let gate = ConsumeGate::default();

        // guide 非空：只取 guide，pending 保持锁定（由随后的 turn 在时机②消费）
        let guide = queue_with(vec![user_entry("g1")]);
        let pending = queue_with(vec![user_entry("p1")]);
        let taken = gate.take(ConsumeTiming::Idle, &guide, &pending);
        assert_eq!(contents(&taken), vec!["g1"]);
        assert_eq!(queue_contents(&pending), vec!["p1"]);

        // guide 空：pending 倒灌解禁（否则只发 pending 会死信）
        let guide = queue_with(vec![]);
        let pending = queue_with(vec![user_entry("p1"), user_entry("p2")]);
        let taken = gate.take(ConsumeTiming::Idle, &guide, &pending);
        assert_eq!(contents(&taken), vec!["p1", "p2"]);
        assert!(pending.lock().unwrap().is_empty());
    }

    /// 许可暂停：上次 turn 中断 / 失败后，任何相位一律空取，双队列原样保留
    #[test]
    fn paused_gate_takes_nothing_at_any_timing() {
        for outcome in [TurnOutcome::Interrupted, TurnOutcome::Failed] {
            let mut gate = ConsumeGate::default();
            gate.on_turn_end(outcome);
            for timing in [
                ConsumeTiming::Idle,
                ConsumeTiming::AfterToolBatch,
                ConsumeTiming::FinalReply,
            ] {
                let guide = queue_with(vec![user_entry("g1")]);
                let pending = queue_with(vec![user_entry("p1")]);
                let taken = gate.take(timing, &guide, &pending);
                assert!(taken.is_empty(), "{outcome:?} 后 {timing:?} 应空取");
                assert_eq!(queue_contents(&guide), vec!["g1"], "guide 剩余应保留");
                assert_eq!(queue_contents(&pending), vec!["p1"], "pending 剩余应保留");
            }
        }
    }

    /// 许可迁移：默认开放；按 turn 退出原因开合；新条目恢复
    #[test]
    fn permission_transitions_on_turn_outcome_and_new_intent() {
        // 默认（引擎启动）：许可开放
        let mut gate = ConsumeGate::default();
        let empty: SharedQueue = queue_with(vec![]);
        let guide = queue_with(vec![user_entry("g0")]);
        assert_eq!(
            contents(&gate.take(ConsumeTiming::Idle, &guide, &empty)),
            vec!["g0"],
            "默认应允许消费"
        );

        // Completed：开放
        gate.on_turn_end(TurnOutcome::Completed);
        let guide = queue_with(vec![user_entry("g1")]);
        assert_eq!(
            contents(&gate.take(ConsumeTiming::Idle, &guide, &empty)),
            vec!["g1"],
            "Completed 后应允许消费"
        );

        // 中断 / 失败：暂停；新条目：恢复
        for outcome in [TurnOutcome::Interrupted, TurnOutcome::Failed] {
            gate.on_turn_end(outcome);
            let guide = queue_with(vec![user_entry("g1")]);
            assert!(
                gate.take(ConsumeTiming::Idle, &guide, &empty).is_empty(),
                "{outcome:?} 后应暂停消费"
            );
            gate.resume_on_new_intent();
            assert_eq!(
                contents(&gate.take(ConsumeTiming::Idle, &guide, &empty)),
                vec!["g1"],
                "{outcome:?} 恢复后应允许消费"
            );
        }

        // Completed 自恢复无变化
        gate.on_turn_end(TurnOutcome::Completed);
        gate.resume_on_new_intent();
        let guide = queue_with(vec![user_entry("g1")]);
        assert_eq!(
            contents(&gate.take(ConsumeTiming::Idle, &guide, &empty)),
            vec!["g1"]
        );
    }
}
