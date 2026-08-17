//! 会话销毁与引擎关闭（单 session 销毁 / 引擎级关闭）
//!
//! 本模块集中 [`Engine`] 的「死亡」相关动作：
//! - [`Engine::end_session`]：销毁单个 session（不影响其他 session）
//! - [`Engine::shutdown`]：关闭整个引擎（cancel 所有 session task 并并发收尾）
//!
//! 两条路径共用私有 helper [`end_one_session`] 完成实际收尾（cancel+超时+abort 兜底），
//! 差异只在「动哪个 token」：end_session 动单个 child_token，shutdown 动引擎 root token。

use super::*;

impl Engine {
    /// 销毁单个对话（动作五）
    ///
    /// 与 [`shutdown`](Self::shutdown) 对称但只针对一个 session：其他 session 不受影响。
    /// 适合 UI 形态需要"关闭某个对话但保留其他对话继续聊"的场景。
    ///
    /// 流程：
    /// 1. shutdown 快路径检查（引擎已关 → end 单 session 无意义，返 `Err(Shutdown)`）
    /// 2. 从调度表移除该 session 的 `SessionHandle`（不存在 → `Err(SessionNotFound)`）
    /// 3. cancel 该 session 的 **child_token**（**不动引擎 root token**，其他 session 不受影响）
    ///    → task 走与 shutdown 完全相同的优雅退出路径（idle select! / turn 中段 select! /
    ///    retry 退避 sleep 全部监听 child_token），退出前调 `store.update(session)` 落 in-flight 状态
    /// 4. 超时（`SHUTDOWN_TASK_TIMEOUT`）等待 task 退出，超时 `abort_handle.abort()` 兜底强杀
    /// 5. task 退出**之后**调 `store.end_session(id, reason)` 填 `ended_at` / `end_reason`
    ///    （时序关键：必须在 task 退出后调，否则会被 task 退出时的全量 `update(session)` 覆盖）
    ///
    /// 与 `shutdown` 的关系：二者共用私有 helper [`end_one_session`] 完成实际收尾，
    /// 差异只在"影响范围"——本方法动一个 child_token，shutdown 动引擎 root token + flag。
    ///
    /// **fire-and-forget task**（如标题生成）：不显式 abort，与 shutdown 一致——靠 runtime 关闭自然终止。
    ///
    /// # 错误
    /// - [`EngineError::Shutdown`]：引擎已 shutdown
    /// - [`EngineError::SessionNotFound`]：session id 不在活跃调度表（已结束或从未创建）
    /// - [`EngineError::Storage`]：落库 `ended_at` / `end_reason` 失败（task 已退出，但元数据未更新）
    pub async fn end_session(&self, id: &SessionId, end_reason: &str) -> Result<(), EngineError> {
        // shutdown 同步快路径检查：引擎已关 → end 单 session 无意义
        if self.shutdown.load(Ordering::Acquire) {
            return Err(EngineError::Shutdown);
        }

        // 从调度表移除（不存在 → SessionNotFound，区分于"引擎已关"）
        let handle = self
            .sessions
            .lock()
            .await
            .remove(id)
            .ok_or_else(|| EngineError::SessionNotFound(id.clone()))?;

        // cancel 该 session 的 **child_token**（不动 root，其他 session 不受影响）
        // → task 的 select! 收到 cancelled 走优雅退出路径（idle/turn/retry 三段都监听 child）
        handle.shutdown_token.cancel();

        // 收尾（超时 await → abort 兜底）
        let outcome = end_one_session(handle).await;
        match outcome {
            SessionExitOutcome::Finished => {
                tracing::info!(
                    session_id = %id,
                    end_reason = end_reason,
                    "session 已正常结束"
                );
            }
            SessionExitOutcome::Panicked(cause) => {
                tracing::warn!(
                    session_id = %id,
                    cause = %cause,
                    "session task panic 退出（已由 task 内 panic 防护或 runtime 兜底）"
                );
            }
            SessionExitOutcome::Aborted => {
                tracing::warn!(
                    session_id = %id,
                    timeout_secs = SHUTDOWN_TASK_TIMEOUT.as_secs(),
                    "session task 超时未退出，已强制 abort（兜底）"
                );
            }
        }

        // task 退出后再写 ended_at / end_reason，保证是最终值
        // （task 退出前的 store.update(session) 落的是当前元数据，ended_at/end_reason 仍为 None；
        //  这里的单字段 UPDATE 把它们写成最终值，不被覆盖）
        self.store.end_session(id, end_reason).await?;
        Ok(())
    }

    /// 关闭引擎
    ///
    /// 引擎关闭是危险操作，不混入对话级的事件流（不走 send），
    /// 由独立的关闭方法触发。
    ///
    /// 关闭流程（「显式关闭 + 等待退出 + 强制中止兜底」三层保障 + 工厂级清理）：
    /// 1. shutdown flag 置位（`AtomicBool::store(true)`）→ 后续 `send` 立即返回 `Err(Shutdown)`，
    ///    `recv` 先 drain 残余事件再返回 None（不丢 shutdown 前最后几条产出）
    /// 2. cancel 引擎级 shutdown_token → 所有 session task 的 select! 同时收到 cancelled 信号
    /// 3. 每个 session task 优雅退出：select! 监听 cancelled → break 主循环 →
    ///    退出前调一次 `store.update(session)` 落库（保护 in-flight 状态，失败仅 warn 不阻塞）
    /// 4. **并发**收尾所有 task（每个 handle spawn 一个 [`end_one_session`] 进 JoinSet）：
    ///    每个 task 独立享 `SHUTDOWN_TASK_TIMEOUT` 超时预算，超时则 `abort_handle.abort()` 兜底强杀；
    ///    收尾内含该 session 插件实例的逆序 dispose（生命周期闭环，见 [`end_one_session`]）。
    ///    并发退出总耗时 ≈ `max(各 task 退出时间)`（JoinSet 同时调度），不受 session 数量影响
    /// 5. 发 INFO 日志（含正常退出 / panic / 超时 abort 计数）
    /// 6. `plugin_host.dispose_all()` 工厂级清理：逆序调用所有 Plugin 工厂 dispose
    ///    （关闭共享连接 / 刷盘等引擎级资源）。无活跃 session 的提前返回分支同样执行——
    ///    工厂资源不依赖 session 存在过。
    ///
    /// **并发等待的理由**：多 session 并发活跃时，各 task 的落库路径会竞争 SQLite WAL 写锁，
    /// 串行 await 会让总耗时退化成 `sum(各 task 退出时间)`；用 JoinSet 并发等待把总耗时压成
    /// `max(各 task 退出时间)`。
    ///
    /// **fire-and-forget task**（如 title 生成等 spawn 的独立 task）：**不显式 abort**，
    /// 靠 runtime 关闭自然终止（已记录决策：fire-and-forget task 不显式 abort）。
    pub async fn shutdown(&self) {
        // 1. flag 置位：后续 send / recv 立即走快路径拒绝
        self.shutdown.store(true, Ordering::Release);

        // 2. cancel 引擎级 token：所有 session task 的 child_token 同时 cancel
        self.shutdown_token.cancel();

        // 3. 取出所有 SessionHandle 的所有权（drain 出 hashmap 才能 move task 进 JoinSet）
        let handles: Vec<(SessionId, SessionHandle)> = {
            let mut sessions = self.sessions.lock().await;
            sessions.drain().collect()
        };

        let total = handles.len();
        if total == 0 {
            // 无活跃 session 也需要清理工厂资源（插件工厂持有引擎级共享依赖）
            self.plugin_host.dispose_all();
            tracing::info!(total = 0, "引擎关闭完成（无活跃 session）");
            return;
        }

        // 4. 并发收尾：每个 handle spawn 一个 end_one_session
        //    （内含 cancel+超时+abort 兜底+插件实例 dispose，与 end_session 单 session 版本共用同一份收尾逻辑）
        let mut set: JoinSet<(SessionId, SessionExitOutcome)> = JoinSet::new();
        for (id, handle) in handles {
            set.spawn(async move {
                let outcome = end_one_session(handle).await;
                (id, outcome)
            });
        }

        let mut finished = 0usize;
        let mut panicked = 0usize;
        let mut aborted_ids: Vec<SessionId> = Vec::new();

        // 收完所有结果（end_one_session 内部已各自有 SHUTDOWN_TASK_TIMEOUT 超时兜底，
        // 这里不再套外层 timeout——并发跑，总耗时 ≈ max(各 task 退出时间)）
        while let Some(joined) = set.join_next().await {
            let (id, outcome) = joined.expect("JoinSet task panic");
            match outcome {
                SessionExitOutcome::Finished => finished += 1,
                SessionExitOutcome::Panicked(cause) => {
                    panicked += 1;
                    tracing::warn!(
                        session_id = %id,
                        cause = %cause,
                        "session task panic 退出（已由 task 内 panic 防护或 runtime 兜底）"
                    );
                }
                SessionExitOutcome::Aborted => aborted_ids.push(id),
            }
        }

        if !aborted_ids.is_empty() {
            tracing::warn!(
                timeout_secs = SHUTDOWN_TASK_TIMEOUT.as_secs(),
                aborted_count = aborted_ids.len(),
                "部分 session task 超时未退出，已强制 abort（兜底）"
            );
        }

        tracing::info!(
            total,
            finished,
            panicked,
            aborted = aborted_ids.len(),
            "引擎关闭完成（所有 session task 已处理）"
        );

        // 5. 工厂级清理：所有 session 收尾完成后逆序 dispose 全部插件工厂
        //    （实例级 dispose 已在各 end_one_session 内完成，这里只清引擎级工厂资源）
        self.plugin_host.dispose_all();
    }
}

/// session task 收尾后的退出结局
///
/// [`end_one_session`] 的返回值，[`Engine::end_session`] 与 [`Engine::shutdown`] 共用：
/// - `Finished`：task 正常退出（task 内 select! 收到 cancelled 后走中断路径落库退出）
/// - `Panicked`：task 以 panic 退出（payload 已转字符串，供 WARN 日志输出）
/// - `Aborted`：task 在 `SHUTDOWN_TASK_TIMEOUT` 内未退出，已 abort 强杀
enum SessionExitOutcome {
    Finished,
    Panicked(String),
    Aborted,
}

/// 单个 session task 的收尾（cancel + 超时等待 + abort 兜底 + 插件实例 dispose）
///
/// `Engine::end_session`（单 session 销毁）与 `Engine::shutdown`（全部 session 销毁）
/// 共用本 helper，保证两条路径的收尾逻辑完全一致——差异只在"动哪个 token"：
/// - `end_session`：在调用方先 cancel 该 session 的 **child_token** 再调本 helper
///   （本 helper 不重复 cancel，避免与"child_token 已 cancel"假设耦合）
/// - `shutdown`：在调用方先 cancel 引擎级 **root token**，所有 child 同时 cancel，
///   然后把每个 handle spawn 进本 helper
///
/// 流程：
/// 1. 拿 `abort_handle`（独立于 `task` 的句柄，超时时用来 abort）
/// 2. `tokio::time::timeout(SHUTDOWN_TASK_TIMEOUT, handle.task)` 等 task 退出
///    - `Ok(Ok(()))`：task 正常完成 → `Finished`
///    - `Ok(Err(e))`：task panic → `Panicked(format_join_error(e))`
///    - `Err(_)`：超时 → `abort_handle.abort()` 强杀 + 等 abort 完成 → `Aborted`
/// 3. task 退出处理后（三个 outcome 分支统一走）：逆序 dispose 该 session 的
///    插件实例（后注册的先销毁），单个实例 panic 不阻塞其余（catch_unwind 防护）
///
/// 注：调用方必须保证进入本 helper 前 `handle.shutdown_token` 已被 cancel
/// （否则 task 可能永远不会退出，纯靠超时 abort 兜底）。本 helper 不自己 cancel
/// 是为了让"cancel 哪个 token"成为调用方决策（child vs root），逻辑更内聚。
async fn end_one_session(handle: SessionHandle) -> SessionExitOutcome {
    // 先拿独立 abort 句柄：超时分支需要它来强杀，而 handle.task 会被 timeout 消费
    let abort_handle = handle.task.abort_handle();
    // 插件实例集合先取出：task 收尾后统一 dispose（与 task 的所有权分离）
    let plugin_instances = handle.plugin_instances;

    let outcome = match tokio::time::timeout(SHUTDOWN_TASK_TIMEOUT, handle.task).await {
        Ok(Ok(())) => SessionExitOutcome::Finished,
        Ok(Err(join_err)) => SessionExitOutcome::Panicked(format_join_error(join_err)),
        Err(_) => {
            // 超时强杀。abort 是异步的——发出信号后立即返回，task 实际终止由 runtime 调度。
            // 不在这里等 abort 完成（AbortHandle 不可 await）；调用方若需确保 task 已终止，
            // 可在更外层（如 Engine::shutdown 退出后 runtime drop）自然回收。
            abort_handle.abort();
            SessionExitOutcome::Aborted
        }
    };

    // dispose 在 task 退出处理完成后统一调用（正常 / panic / 超时 abort 三分支都走）：
    // 保证插件观察到的 session 状态已落定（task 内最后的落库与状态更新已结束）。
    // 逆序（后注册的先 dispose）与装配顺序对称，逐个调用，单个 panic 记 WARN 后继续
    for (name, instance) in plugin_instances.into_iter().rev() {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| instance.dispose()));
        if let Err(payload) = result {
            tracing::warn!(
                plugin = %name,
                phase = "dispose",
                recovered = true,
                cause = %fuyao_hooks::panic_payload_to_string(&*payload),
                "插件实例 dispose panic 已恢复"
            );
        }
    }

    outcome
}

/// 把 `JoinError` 格式化为可读字符串（用于日志）
///
/// panic 类型的任务退出原因通常含 payload，转字符串供 WARN 日志输出。
/// owned 传入：`try_into_panic` 消费 JoinError。
fn format_join_error(err: JoinError) -> String {
    if err.is_panic() {
        match err.try_into_panic() {
            Ok(payload) => fuyao_hooks::panic_payload_to_string(&*payload),
            Err(_) => "task panic（payload 不可恢复）".to_string(),
        }
    } else if err.is_cancelled() {
        "task 被取消".to_string()
    } else {
        format!("task 退出异常: {err}")
    }
}
