//! App 装配：fan-in 多个主 session 的 per-session rx 为单一出口
//!
//! 装配层承担 fan-in 职责，重建单一出口。
//! - [`App`] 持有 [`Engine`] + fan_out 通道（bounded）+ forward_tasks 表
//! - 每个**主 session**（create_session / resume_session）创建时 spawn 一个 forwarder
//! - 暴露 [`App::recv`] 给上层消费者，语义等价于原 `Engine::recv`
//! - [`App::shutdown`] 两段式：engine.shutdown 等 session task 退出 → forwarder 自然退出 → drop fan_out_tx
//!
//! **子 session**（create_child_session）不进 fan-out——rx 直接返调用方独占消费
//! （子任务的事件不暴露给 UI 出口，由调用方按业务消费）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use fuyao_api::message::OutputEvent;
use fuyao_api::{AgentConfig, InputEvent, SessionParams};
use fuyao_core::{ChildSessionSource, Engine, EngineError, SessionId};
use fuyao_mcp::MCPManager;
use tokio::sync::{Mutex, mpsc};
use tokio::task::{JoinHandle, JoinSet};

use crate::bootstrap::LogGuard;

/// shutdown 等 forwarder task 退出的总超时阈值
///
/// 与 `core::engine::SHUTDOWN_TASK_TIMEOUT` 一致——forwarder 在 session task 退出后
/// 几毫秒内自然退出（rx 返 None），10s 是极端兜底（forwarder panic 卡死等场景）。
const SHUTDOWN_FORWARD_TIMEOUT: Duration = Duration::from_secs(10);

/// 应用装配产物：持引擎 + fan-in 出口
///
/// 装配层（fuyao-app）承担 fan-in 职责，把所有常规 session 的 per-session rx 汇聚到
/// 单一 fan_out 通道，通过 [`App::recv`] 对外暴露统一的出口。
///
/// # 生命周期
/// - [`App::new`]：手工装配（测试 / 不走 [`crate::start`] 的场景）
/// - [`App::create_session`] 等：内部 spawn forwarder，返 `session_id`（**不返 rx**——
///   rx 由内部 forwarder 消费）
/// - [`App::recv`]：单一出口消费所有常规 session 的事件
/// - [`App::shutdown`]：两段式收尾（engine.shutdown → forwarder 自然退出 → drop fan_out_tx）
///
/// # 子 session 不进 fan_out
/// [`App::create_child_session`] 返 `(id, rx)`——rx 由调用方独占消费，不进 fan_in。
/// 同步子代理 tool handler / fire-and-forget 后台任务均用此路径。
pub struct App {
    /// 引擎内核（`Arc` 共享：handler 闭包捕获弱引用注入工具 ctx；App 通过 Deref 代理 API）
    engine: Arc<Engine>,
    /// MCP 管理器（无配置 server 时为 None）。shutdown 时调 stop_all 优雅关闭。
    mcp_manager: Option<Arc<MCPManager>>,
    /// 日志 guard：drop 时 flush 文件缓冲，须存活到 App 结束。
    ///
    /// 仅靠 drop 副作用（flush 缓冲）生效，从不直接读取——用 `#[allow(dead_code)]`
    /// 抑制未读取警告（与 `LogGuard.file_guard` 同型）。
    #[allow(dead_code)]
    log_guard: LogGuard,
    /// fan_out 发送端（App 持一份，每个 forwarder 各持一份 clone）
    ///
    /// 所有 forwarder 退出 + App shutdown drop 此字段后，fan_out_rx 后续 recv 返 None。
    fan_out_tx: mpsc::Sender<OutputEvent>,
    /// fan_out 接收端（[`App::recv`] 消费，跨 await 持锁用 Mutex 包，与原 Engine::rx_event 同型）
    fan_out_rx: Mutex<mpsc::Receiver<OutputEvent>>,
    /// 每 session 一个 forwarder 句柄（shutdown / destroy_session 时收尾）
    forward_tasks: Mutex<HashMap<SessionId, JoinHandle<()>>>,
}

impl App {
    /// 手工装配（测试 / 不走 [`crate::start`] 的场景用）
    ///
    /// 调用方负责先把 `engine` / `mcp_manager` / `log_guard` 准备好。
    /// [`crate::start`] 内部也是调本方法（多走一遍 init + 工具收集）。
    pub fn new(
        engine: Arc<Engine>,
        mcp_manager: Option<Arc<MCPManager>>,
        log_guard: LogGuard,
    ) -> Self {
        // fan_out 通道（进程级消费缓冲），容量取自全局 [engine].fan_out_capacity。
        // 两级缓冲：per-session 出站为无界通道，吸收单 session 的瞬时突发（流式 chunk 等）；
        // fan_out 是所有 session 共享的有界汇聚缓冲，给 [`App::recv`] 的消费方留出平滑余量。
        // 有界而非无界：消费方长期不消费时（UI 卡死等）事件不无限堆积占内存；背压只落在
        // forwarder（阻塞 send），不回传导 session task（per-session 无界仍在接）。
        // 默认 512 为经验值，覆盖多 session 并发流式输出下的消费抖动；实测不够经配置调大。
        let (fan_out_tx, fan_out_rx) =
            mpsc::channel::<OutputEvent>(fuyao_api::get_config().engine.fan_out_capacity);
        Self {
            engine,
            mcp_manager,
            log_guard,
            fan_out_tx,
            fan_out_rx: Mutex::new(fan_out_rx),
            forward_tasks: Mutex::new(HashMap::new()),
        }
    }

    /// 创建对话（返 session_id，rx 由内部 forwarder 消费进 fan_out）
    ///
    /// 镜像 [`Engine::create_session`]：内部调 engine 拿 `(id, rx)` → spawn forwarder
    /// 把 rx 转到 fan_out → 返 id（**不返 rx**）。
    pub async fn create_session(&self, params: SessionParams) -> Result<SessionId, EngineError> {
        let (id, rx) = self.engine.create_session(params).await?;
        self.register_forwarder(id.clone(), rx).await;
        Ok(id)
    }

    /// 恢复对话（幂等：已挂载则零成本直返，未挂载才读 DB 装配）
    ///
    /// 幂等短路：若该 session 的 forwarder 已在跑（`forward_tasks` 命中），说明 session
    /// 已挂载、事件流已通向 fan_out——直接返回 id，不读 DB、不重新装配、不重建
    /// forwarder。这使得「每次发送前先 resume」成为零成本操作：首条消息时挂载，后续
    /// 命中短路瞬间返回。
    ///
    /// 未挂载（重启后 / 跨进程）才走 [`Engine::resume_session`]：读 DB 取 session
    /// 元数据 → 装配 task + 通道 → 返 rx → 经 [`register_forwarder`] 接进 fan_out。
    ///
    /// session id 不在数据库 → [`EngineError::SessionNotFound`]（由 Engine 层返回）。
    pub async fn resume_session(
        &self,
        id: &SessionId,
        params: SessionParams,
    ) -> Result<SessionId, EngineError> {
        // 幂等短路：forwarder 已在跑 → session 已挂载、事件流已通 → 零成本直返
        if self.forward_tasks.lock().await.contains_key(id) {
            return Ok(id.clone());
        }
        let (id, rx) = self.engine.resume_session(id, params).await?;
        self.register_forwarder(id.clone(), rx).await;
        Ok(id)
    }

    /// 创建子任务 session（rx **不进 fan_out**，直接返调用方独占消费）
    ///
    /// 镜像 [`Engine::create_child_session`]——子任务 session 的事件不暴露给 UI 出口，
    /// rx 由调用方独占消费：
    /// - 同步子代理 tool handler：消费 rx 取 `finish_reason=stop` 的最终回复
    /// - fire-and-forget 后台任务：spawn 独立 task 消费 rx（写日志 / 丢弃均可——事件已落库）
    pub async fn create_child_session(
        &self,
        parent_session_id: &SessionId,
        source: ChildSessionSource,
        child_agent_config: AgentConfig,
    ) -> Result<(SessionId, mpsc::UnboundedReceiver<OutputEvent>), EngineError> {
        // 不 spawn forwarder，rx 直接返调用方独占消费
        self.engine
            .create_child_session(parent_session_id, source, child_agent_config)
            .await
    }

    /// 更新对话级参数（运行时配置热切，直接代理 [`Engine::update_session_params`]）
    ///
    /// 与 [`App::resume_session`](Self::resume_session) 职责正交：resume 负责会话装配加载
    ///（从 DB 读、重建队列/通道），本方法负责运行中会话的配置热切——覆盖 session 的
    /// `SessionParams` 共享句柄，消费点（跑 turn、压缩）下次现读即用新值。
    ///
    /// 全量覆盖语义：无论传入什么 `SessionParams`，整份直接替代当前值——agent_config /
    /// model_config 一视同仁。session 不在调度表返 [`EngineError::SessionNotFound`]。
    pub async fn update_session_params(
        &self,
        id: &SessionId,
        params: SessionParams,
    ) -> Result<(), EngineError> {
        self.engine.update_session_params(id, params).await
    }

    /// 入事件（单一入口，直接代理 [`Engine::send`]）
    pub async fn send(&self, id: &SessionId, event: InputEvent) -> Result<(), EngineError> {
        self.engine.send(id, event).await
    }

    /// 按客户端消息标识删除双队列中未消费的消息（直接代理 [`Engine::remove_queued_message`]）
    ///
    /// 返回删除条数（0 = 队列中无此标识的消息）。与 [`App::send`](Self::send) 组合使用：
    /// 上层为每条发送的用户消息自配 `client_message_id`，撤回排队消息时凭该标识定位删除。
    /// session 不在调度表返 [`EngineError::SessionNotFound`]。
    pub async fn remove_queued_message(
        &self,
        id: &SessionId,
        client_message_id: &str,
    ) -> Result<usize, EngineError> {
        self.engine
            .remove_queued_message(id, client_message_id)
            .await
    }

    /// 停止会话当前 turn（屏障语义，直接代理 [`Engine::stop_session`]）
    ///
    /// 返回即该 session 的 DB 已静默（在跑 turn 已完全终止、中断收尾落库完毕），
    /// session 保持存活——与 [`destroy_session`](Self::destroy_session) 的销毁语义正交。
    /// 管理型同步方法，不走消息总线。
    ///
    /// 典型消费方是「先停后改库」的应用编排（如数据库回退）：本方法成功返回后，
    /// 再经 [`SessionManager`](crate::SessionManager) 做存储层写操作，两步之间
    /// 不存在该 session 的并发写库。
    pub async fn stop_session(&self, id: &SessionId, reason: &str) -> Result<(), EngineError> {
        self.engine.stop_session(id, reason).await
    }

    /// 刷新存活 engine 的供应商注册表（直接代理 [`Engine::reload_providers`]）
    ///
    /// 供应商落盘变更后的内存对齐原语：把注册表重建为当前落盘状态（新增 /
    /// 修改刷新实例、删除清除实例与缓存），下一 turn 生效。同步执行（纯
    /// CPU + 本地文件读，无 await 点）。多 engine 场景的全池刷新编排属上层
    /// 职责——每台 engine 各调一次本方法。
    ///
    /// # 返回
    /// 实例构造被跳过的供应商 id 名单（空 = 全部成功注册）。
    ///
    /// # 错误
    /// 三层配置加载失败 → [`EngineError::Provider`]，信息直指排查方向。
    pub fn reload_providers(&self) -> Result<Vec<String>, EngineError> {
        self.engine.reload_providers()
    }

    /// 出站事件单一出口（fan_out 接收端）
    ///
    /// 从 fan_out 通道消费事件——所有常规 session 的 forwarder 都把事件推到这里。
    /// 与原 `Engine::recv` 等价：阻塞等待下一条事件，所有 sender drop 后返 None。
    ///
    /// 调用方按 `event.base.session_id` 归类分流（per-session FIFO，跨 session 不指定顺序）。
    pub async fn recv(&self) -> Option<OutputEvent> {
        self.fan_out_rx.lock().await.recv().await
    }

    /// 出站通道发送端的克隆（app 级事件汇入口）
    ///
    /// 供不经 session task 的产出方向单一出口投递纯事件（如会话管理门面的
    /// 文件回退结果）：发送端克隆与 forwarder 共用同一 fan_out 通道，
    /// [`recv`](Self::recv) 的消费方不区分事件来自 forwarder 还是本入口。
    /// 有界通道——消费方停滞时 send 阻塞（与 forwarder 同一背压点）。
    pub fn event_sink(&self) -> mpsc::Sender<OutputEvent> {
        self.fan_out_tx.clone()
    }

    /// 销毁单个 session（调 [`Engine::destroy_session`] + 单独收尾该 session 的 forwarder）
    ///
    /// 流程：
    /// 1. `engine.destroy_session(id)`：cancel child_token → session task 退出 → tx_session drop
    /// 2. 从 `forward_tasks` 取出该 session 的 forwarder handle，超时 + abort 兜底收尾
    ///    （forwarder 此时 rx 返 None 几毫秒内退；超时 abort 兜底防卡死）
    ///
    /// 与 [`App::shutdown`] 对称但只针对一个 session：其他 session 不受影响。
    ///
    /// # 错误
    /// 透传 [`Engine::destroy_session`] 的错误（`Shutdown` / `SessionNotFound`）。
    /// 出错时不收尾 forwarder（engine 层未实际销毁 session，forward_tasks 表不动）。
    pub async fn destroy_session(&self, id: &SessionId, reason: &str) -> Result<(), EngineError> {
        // 段 1：engine 收尾 session task（cancel + 超时 abort，task 退出即完成）
        self.engine.destroy_session(id, reason).await?;

        // 段 2：单独收尾该 session 的 forwarder（task 退出后 forwarder rx 返 None 自然退；
        //       超时 abort 兜底，与 core::end_one_session 模式一致但 app 层独立实现）
        let handle = self.forward_tasks.lock().await.remove(id);
        if let Some(handle) = handle {
            end_one_forward_task(id.clone(), handle).await;
        }
        Ok(())
    }

    /// 关闭 App（两段式收尾 + MCP 停机 + drop log_guard）
    ///
    /// 流程（shutdown 时序）：
    /// 1. `engine.shutdown()` → session task 全部退出 → tx_session drop → rx 返 None
    /// 2. 等 forward_tasks 全部退出（并发 JoinSet + 各自 `SHUTDOWN_FORWARD_TIMEOUT` 超时 abort 兜底）
    /// 3. 停 MCP（`mcp_manager.stop_all()` 优雅关闭所有 server 连接）
    /// 4. self drop → drop fan_out_tx（fan_out_rx 后续 recv 返 None）+ drop log_guard（flush 文件日志）
    ///
    /// **消费 self**——shutdown 后调用方失去 App 句柄，不能再 send / recv。
    /// 与原 `fuyao_app::shutdown(engine, ctx)` 一致：消费形态，无残留状态。
    pub async fn shutdown(self) {
        // 段 1：engine shutdown 等 session task 退出（tx_session drop 触发 forwarder rx 返 None）
        self.engine.shutdown().await;

        // 段 2：等所有 forwarder 退出（并发 JoinSet + 各自超时 abort 兜底）
        //       drain 出 hashmap 才能 move handle 进 JoinSet（与 Engine::shutdown 同型）
        let handles: Vec<(SessionId, JoinHandle<()>)> = {
            let mut tasks = self.forward_tasks.lock().await;
            tasks.drain().collect()
        };
        if !handles.is_empty() {
            let total = handles.len();
            let mut set: JoinSet<()> = JoinSet::new();
            for (id, handle) in handles {
                set.spawn(end_one_forward_task(id, handle));
            }
            // 收完所有结果（end_one_forward_task 内部已各自有超时 abort 兜底 + WARN 日志，
            // 这里只 await 到全部退出，不重复统计）
            while set.join_next().await.is_some() {}
            tracing::info!(total, "所有 forwarder task 已处理");
        }

        // 段 3：停 MCP（partial move self.mcp_manager）
        if let Some(mcp_manager) = self.mcp_manager {
            mcp_manager.stop_all().await;
        }

        // 段 4：self drop → drop fan_out_tx + fan_out_rx + log_guard（flush 文件日志）
        //       drop fan_out_tx 后 fan_out_rx 后续 recv 会返 None（理论语义，self 已 drop）
        tracing::info!("应用已优雅关闭（引擎 + forwarder + MCP 已停）");
    }

    /// 为指定 session spawn forwarder task，登记进 forward_tasks 表
    ///
    /// forwarder 把该 session 的 rx 转到共享 fan_out_tx（fan-in）。
    /// JoinHandle 登记进 forward_tasks，shutdown / destroy_session 时收尾。
    ///
    /// 理论上 session_id 唯一不冲突；若发现同 id 已存在（理论 bug），abort 旧 handle 防泄漏。
    async fn register_forwarder(
        &self,
        session_id: SessionId,
        rx: mpsc::UnboundedReceiver<OutputEvent>,
    ) {
        let handle = tokio::spawn(forward_events(
            session_id.clone(),
            rx,
            self.fan_out_tx.clone(),
        ));
        if let Some(prev) = self.forward_tasks.lock().await.insert(session_id, handle) {
            // 理论不会发生（session_id 唯一）——发现旧 handle 说明逻辑 bug，abort 防泄漏
            tracing::warn!(
                "forward_tasks 表中已存在该 session_id 的 handle（理论不应发生），已 abort 旧 handle"
            );
            prev.abort();
        }
    }
}

/// forwarder task 主体：把一个 session 的 rx 转到共享 fan_out_tx（fan-in）
///
/// 循环 `rx.recv()`：rx 返 Some → 推到 fan_out；fan_out_tx 关闭（app 异常退出）→
/// break + WARN（决策 5：不触发引擎 shutdown，core 不替 app 兜底）。
///
/// rx 返 None（session task 退出 → tx_session drop）时自然退出循环。
async fn forward_events(
    session_id: SessionId,
    mut rx: mpsc::UnboundedReceiver<OutputEvent>,
    fan_out_tx: mpsc::Sender<OutputEvent>,
) {
    while let Some(ev) = rx.recv().await {
        if fan_out_tx.send(ev).await.is_err() {
            tracing::warn!(session_id = %session_id, "fan-out 通道已关闭，forwarder 退出");
            break;
        }
    }
}

/// 单个 forwarder task 的收尾（超时等待 + abort 兜底）
///
/// 与 `core::engine::end_one_session` 模式一致——app 层独立实现，不污染 core 边界
/// （决策 8.6：end_one_session 不改 pub）。
///
/// 流程：
/// 1. 拿 `abort_handle`（独立于 `handle` 的句柄，超时分支用来强杀）
/// 2. `tokio::time::timeout(SHUTDOWN_FORWARD_TIMEOUT, handle)` 等 task 退出
///    - `Ok(Ok(()))`：正常完成
///    - `Ok(Err(e))`：panic → WARN 日志
///    - `Err(_)`：超时 → `abort_handle.abort()` 强杀 → WARN 日志
///
/// 注：调用方需保证进入本 fn 前 session task 已退出（rx 返 None），否则 forwarder
/// 卡在 `rx.recv().await`，只能靠超时 abort 兜底。
async fn end_one_forward_task(session_id: SessionId, handle: JoinHandle<()>) {
    let abort_handle = handle.abort_handle();
    match tokio::time::timeout(SHUTDOWN_FORWARD_TIMEOUT, handle).await {
        Ok(Ok(())) => {
            tracing::debug!(session_id = %session_id, "forwarder task 正常退出");
        }
        Ok(Err(join_err)) => {
            tracing::warn!(
                session_id = %session_id,
                cause = %join_err,
                "forwarder task panic 退出"
            );
        }
        Err(_) => {
            abort_handle.abort();
            tracing::warn!(
                session_id = %session_id,
                timeout_secs = SHUTDOWN_FORWARD_TIMEOUT.as_secs(),
                "forwarder task 超时未退出，已强制 abort（兜底）"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_core::ToolRegistry;
    use fuyao_hooks::PluginHost;

    /// 构造测试用 AgentPaths（临时 fuyao_home + 唯一 agent_id，隔离进程级注册缓存）
    fn unique_paths(test_name: &str, home: &std::path::Path) -> fuyao_api::AgentPaths {
        fuyao_api::AgentPaths {
            agent_id: Some(format!("global/{test_name}")),
            workspace: None,
            extra_dirs: Vec::new(),
            fuyao_home: home.to_path_buf(),
        }
    }

    /// 落盘一份含目标供应商的 global 层 fuyao.toml（明文 api_key 走 options，
    /// 避免 .env 环境变量串扰）
    fn write_global_config(home: &std::path::Path, provider_id: &str) {
        let content = format!(
            "[providers.{provider_id}]\n\
             name = \"Test\"\n\
             api_protocol = \"openai-completions\"\n\
             options = {{ api_key = \"sk-test\" }}\n\
             [providers.{provider_id}.models.\"m1\"]\n\
             name = \"m1\"\n\
             limit = {{ context = 64000 }}\n"
        );
        std::fs::write(home.join("fuyao.toml"), content).expect("写测试配置失败");
    }

    /// 构造最小 App：空注册表 + 空工具表 + 空插件的裸 engine + 空 MCP + 空 log guard
    ///
    /// 供应商运行时注册不依赖装配期实例，裸 registry 起点即可验证代理链路。
    async fn bare_app(paths: fuyao_api::AgentPaths) -> App {
        let store = std::sync::Arc::new(
            fuyao_session::SessionStore::new(paths.sessions_db_path())
                .await
                .expect("构造 SessionStore 失败"),
        );
        let engine = Engine::new(
            fuyao_api::EngineParams {
                agent_paths: paths.clone(),
            },
            fuyao_provider::ProviderRegistry::default(),
            ToolRegistry::builder().build(),
            PluginHost::new(),
            store,
            fuyao_snapshot::FileSnapshot::disabled(),
        )
        .await;
        App::new(engine, None, LogGuard::default())
    }

    /// 代理 reload_providers：调用抵达内部 engine（落盘供应商注册进缓存 +
    /// 实例入表；落盘删除的清除），成功返回即完成全链路对齐
    #[tokio::test]
    async fn reload_providers_proxy_aligns_internal_engine() {
        let home = tempfile::tempdir().unwrap();
        let paths = unique_paths("app_proxy_reload", home.path());
        write_global_config(home.path(), "alpha");
        let app = bare_app(paths.clone()).await;

        let skipped = app.reload_providers().expect("代理刷新应成功");

        assert!(skipped.is_empty(), "key 齐备不应有跳过：{skipped:?}");
        // 刷新链路把 Provider 与旗下 Model 写进进程级缓存（按路径身份隔离），
        // 缓存可查是刷新已执行的公开可见证据
        assert!(
            fuyao_provider::get_provider("alpha", &paths).is_some(),
            "Provider 配置缓存应可查"
        );
        assert!(
            fuyao_provider::get_model("alpha/m1", &paths).is_some(),
            "模型缓存条目应可查"
        );

        fuyao_provider::clear_cache(&paths);
    }
}
