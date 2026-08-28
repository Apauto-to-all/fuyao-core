//! 会话停止（屏障语义）
//!
//! [`Engine::stop_session`]：打断指定 session 在跑的 turn，并**等到它完全终止**
//! （含中断收尾的补发落库）才返回——返回即该 session 的 DB 已静默。
//!
//! 与 [`teardown`](super::teardown) 模块的 `destroy_session`（会话销毁）正交：
//! stop 后 session 保持存活（task 不退出、调度表不移除），
//! 只是清空在途运行。典型消费方是应用编排层的「先停后改库」两步组合
//! （如数据库回退：先 stop 屏障停 turn，再做存储层写操作）。

use super::*;
use fuyao_api::InterruptSource;

/// stop_session 等待 turn 静默的超时预算
///
/// 停止信号送达后，turn 的收尾（中断通知事件 + 部分结果补发落库）通常毫秒级完成；
/// 预算主要覆盖不监听中断通道的环节（pre-turn 压缩的 LLM 调用、不响应取消的工具）。
/// 到点仍未静默返回 `Err(StopTimeout)`——调用方不得继续依赖「已静默」前提
/// 做后续 DB 操作。
const STOP_TURN_TIMEOUT: Duration = Duration::from_secs(10);

impl Engine {
    /// 停止会话当前 turn（屏障语义：返回即该 session 的 DB 已静默）
    ///
    /// # 屏障语义
    ///
    /// 「返回 = 无在途 turn 且收尾落库已全部完成」。实现基础：session task 是
    /// 该 session DB 写入的唯一执行者，turn 相位（`TurnPhase`）回 `Idle` 即代表
    /// 本 session 已无任何在途写库——中断收尾的补发（部分 AssistantMessage /
    /// 中断式 ToolResult）在 turn 返回前已落库，相位回落晚于全部落库。
    ///
    /// # 幂等语义（「确保静默」，而非「必须有个 turn 可停」）
    ///
    /// - session 不在调度表（未挂载 / 已结束）→ 无 task 即无 DB 写入者，静默前提
    ///   天然成立，直接 `Ok(())`——与 `destroy_session` 对缺失 id 报 `SessionNotFound`
    ///   的差异是刻意的：销毁是生命周期迁移（目标不存在是错误），停止是静默保证
    ///   （目标不存在则保证平凡成立）
    /// - 相位已 `Idle`（无 turn 在跑）→ 直接 `Ok(())`
    /// - turn 在跑 → 经中断通道投递停止信号（`InterruptSource::Stop`），等相位回 `Idle`
    ///
    /// # 超时
    ///
    /// `STOP_TURN_TIMEOUT` 内未等到静默 → `Err(EngineError::StopTimeout)`。
    /// 调用方应放弃依赖静默前提的后续操作（如数据库回退），可稍后重试停止。
    ///
    /// # 已知无害竞争
    ///
    /// 相位判定为 `Running` 后、停止信号送达前 turn 恰好自然结束：信号滞留通道由
    /// idle 段消费，产生一条「idle 中断」通知事件——无 DB 影响，可接受。
    ///
    /// # 错误
    /// - [`EngineError::Shutdown`]：引擎已 shutdown
    /// - [`EngineError::StopTimeout`]：超时未等到 turn 静默
    pub async fn stop_session(&self, id: &SessionId, reason: &str) -> Result<(), EngineError> {
        // shutdown 同步快路径检查：引擎已关 → 无可停对象
        if self.shutdown.load(Ordering::Acquire) {
            return Err(EngineError::Shutdown);
        }

        // 锁作用域内仅取出投递句柄与相位接收端（clone 廉价），锁外投递与等待——
        // 与 send 同款锁纪律：背压 / 长等待绝不持调度表锁
        let (tx_interrupt, mut phase_rx) = {
            let sessions = self.sessions.lock().await;
            let Some(handle) = sessions.get(id) else {
                // 不在调度表：无 task = 无 DB 写入者，静默前提平凡成立
                tracing::debug!(session_id = %id, "stop_session：session 未挂载，无需停止");
                return Ok(());
            };
            (handle.tx_interrupt.clone(), handle.turn_phase_rx.clone())
        };

        // 相位已 Idle：无 turn 在跑，幂等直返
        if !matches!(*phase_rx.borrow(), TurnPhase::Running) {
            tracing::debug!(session_id = %id, "stop_session：无在跑 turn，幂等直返");
            return Ok(());
        }

        // 经中断通道投递停止信号（与 send 的 Interrupt 分流同语义：满则背压、断则 Shutdown）。
        // 发送失败说明 rx 已 drop（task 已退出）——task 退出即无写入者，静默成立。
        let interrupt_msg = OutputInterruptMessage::new(reason.to_string(), InterruptSource::Stop);
        if tx_interrupt.send(interrupt_msg).await.is_err() {
            tracing::debug!(session_id = %id, "stop_session：task 已退出，静默平凡成立");
            return Ok(());
        }

        // 等相位回 Idle（watch 标准循环：先查后等，无丢失唤醒）。
        // watch sender drop（task 已退出）同样代表无写入者，视作静默达成。
        let wait_quiesce = async {
            loop {
                if !matches!(*phase_rx.borrow_and_update(), TurnPhase::Running) {
                    return;
                }
                if phase_rx.changed().await.is_err() {
                    return;
                }
            }
        };
        if tokio::time::timeout(STOP_TURN_TIMEOUT, wait_quiesce)
            .await
            .is_err()
        {
            tracing::warn!(
                session_id = %id,
                timeout_secs = STOP_TURN_TIMEOUT.as_secs(),
                "stop_session 等待 turn 静默超时（turn 可能卡在不响应中断信号的环节）"
            );
            return Err(EngineError::StopTimeout {
                session_id: id.clone(),
                timeout_secs: STOP_TURN_TIMEOUT.as_secs(),
            });
        }

        tracing::info!(
            session_id = %id,
            reason = reason,
            "会话 turn 已停止且收尾落库完毕（屏障达成）"
        );
        Ok(())
    }
}
