//! 影子仓对象回收的后台任务
//!
//! 快照对象不写 ref、7 天 TTL 自然过期，磁盘占用有界靠周期 gc 兜底：引擎启动后
//! 后台跑一次 + 每 24h 一次 `git gc --prune=7.days`。tokio spawn 启动即返回
//! （不阻塞引擎装配），退出信号挂引擎级 shutdown_token——`Engine::shutdown`
//! cancel root token 时任务即时退出（fire-and-forget 形态，不持有 JoinHandle，
//! 与标题生成等既有后台任务一致）。
//!
//! 禁用态快照器的 gc 是跳过成功（no-op），任务照常按周期空转（每 24h 一次
//! 零成本 tick）。gc 失败只 WARN：回收是磁盘卫生问题，不影响任何功能正确性——
//! 失败的周期等下个周期自然重试。

use fuyao_snapshot::FileSnapshot;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// 两次 gc 之间的间隔
const GC_INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// 启动影子仓 gc 后台任务（启动即后台跑一次，此后每 24h 一次）
pub(super) fn spawn_snapshot_gc(snapshot: FileSnapshot, shutdown: CancellationToken) {
    tokio::spawn(run_snapshot_gc(snapshot, shutdown));
}

/// gc 循环主体：interval 首个 tick 立即完成（启动即跑一次），此后每 24h 一轮；
/// shutdown 信号优先胜出，任意等待阶段收到即退出
async fn run_snapshot_gc(snapshot: FileSnapshot, shutdown: CancellationToken) {
    let mut interval = tokio::time::interval(GC_INTERVAL);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                tracing::debug!("影子仓 gc 任务收到退出信号");
                break;
            }
            _ = interval.tick() => {
                if let Err(cause) = snapshot.gc().await {
                    tracing::warn!(
                        cause = %cause,
                        "影子仓 gc 失败（不影响功能，等待下个周期重试）"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 禁用态快照器：gc 任务首个周期空转成功，shutdown 即退出
    #[tokio::test]
    async fn gc_task_noops_on_disabled_snapshot_and_exits_on_shutdown() {
        let snapshot = FileSnapshot::disabled();
        let token = CancellationToken::new();
        let handle = tokio::spawn(run_snapshot_gc(snapshot, token.clone()));
        // 首个 tick 立即完成，稍候任务已进入等待（无 panic、无报错）
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!handle.is_finished(), "任务应在等待下个周期而非退出");
        token.cancel();
        handle
            .await
            .expect("gc 任务不应 panic（禁用态 gc 跳过成功）");
    }
}
