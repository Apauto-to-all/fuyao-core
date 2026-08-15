//! 会话运行时操作（入站分发 / 参数更新）
//!
//! 本模块集中 [`Engine`] 的「运行」相关动作：
//! - [`Engine::send`]：入站事件单一入口（User / Interrupt 分流）
//! - [`Engine::update_session_params`]：运行时调整 session 参数
//!
//! 出站事件不再走 Engine——每 session 持自己的 per-session 通道，rx 由创建方法
//! 返调用方独占消费。

use super::*;

/// [`Engine::send`] 的分流产物：目标通道 + 载荷
///
/// 调度表锁作用域内只做查表与消息转化，取出 sender（clone 廉价）即释放锁，
/// 通道 send 的背压等待发生在锁外——session 通道均有界，通道满时 send 会挂起，
/// 若此时仍持引擎级 sessions 锁，会把 send / create / shutdown 全部排队，
/// 一个 session 的背压拖死所有 session（跨 session 队头阻塞）。
enum OutboundAction {
    /// User 消息 → 入站通道
    Inbound(
        mpsc::Sender<fuyao_api::message::output::UserMessage>,
        fuyao_api::message::output::UserMessage,
    ),
    /// 中断信号 → 中断通道
    Interrupt(mpsc::Sender<OutputInterruptMessage>, OutputInterruptMessage),
    /// 控制命令（手动压缩 / 回退）→ 控制通道
    Control(
        mpsc::Sender<fuyao_api::ControlCommand>,
        fuyao_api::ControlCommand,
    ),
}

/// 通道投递（send 的统一语义包装）
///
/// 三类分流通道共用同一语义：通道满则背压等待，通道断开（session task 已退出）
/// 视为引擎已关停，报 `EngineError::Shutdown`。载荷类型不同由泛型吸收。
async fn deliver<T>(tx: mpsc::Sender<T>, msg: T) -> Result<(), EngineError> {
    tx.send(msg).await.map_err(|_| EngineError::Shutdown)
}

impl Engine {
    /// 入事件（单一入口）
    ///
    /// 所有对话级输入事件从一个口进，靠 session id 区分对话，按事件类型分流：
    /// - `User`：入队，触发 ReAct 循环。模型配置从 session 的 `SessionParams` 现读（见 [`update_session_params`](Self::update_session_params)）
    /// - `Interrupt`：发出中断信号，打断对应对话的当前执行
    /// - `Compress` / `Rollback`：控制类命令，转 ControlCommand 投控制通道，task 在 turn 边界自执行；
    ///   结果经 per-session 出口以对应 OutputEvent 流出（Compression / Rollback）
    ///
    /// 入队即返回，不阻塞——不等大模型想完。
    /// 后续产出从该 session 的 per-session rx 流出（由 create_session 等返回）。
    ///
    /// session id 不在调度表 → 同步返回 `Err(SessionNotFound)`（要恢复走恢复动作）。
    pub async fn send(&self, id: &SessionId, event: InputEvent) -> Result<(), EngineError> {
        // shutdown 同步快路径检查：已关闭立即拒绝（区分于 SessionNotFound）
        if self.shutdown.load(Ordering::Acquire) {
            return Err(EngineError::Shutdown);
        }

        // 锁作用域内完成查表与消息转化，取出目标 sender 后释放调度表锁；
        // 通道 send 的背压等待移到锁外（见 OutboundAction 文档），入队即返回语义不变。
        let action = {
            let sessions = self.sessions.lock().await;
            let handle = sessions
                .get(id)
                .ok_or_else(|| EngineError::SessionNotFound(id.clone()))?;

            match event {
                InputEvent::User(user_msg) => {
                    // 立即把 input 侧 UserMessage 字段照搬转化为 output 侧 UserMessage
                    // （base + payload 完整保留，含 source），直接送进 session task。
                    // 模型/思考等运行时配置挂在 session 级（SessionParams.model_config），
                    // 消费点（跑 turn、压缩）现读现用，不随消息携带。
                    // 后续 handle_inbound_user 纯入队，inject_messages 消费时统一过管道。
                    let outbound = fuyao_api::message::output::UserMessage {
                        base: user_msg.base,
                        payload: fuyao_api::message::output::UserPayload {
                            content: user_msg.payload.content,
                            images: user_msg.payload.images,
                            mode: user_msg.payload.mode,
                            source: user_msg.payload.source,
                        },
                    };
                    OutboundAction::Inbound(handle.tx_inbound.clone(), outbound)
                }
                InputEvent::Interrupt(interrupt_msg) => {
                    // 入口转化：input 侧 InterruptMessage → output 侧 InterruptMessage。
                    // input 侧消息的唯一职责就是在此被转化，之后内核链路（通道、select!、
                    // 中断通知与收尾入口）全程只认 output 侧类型。
                    let outbound = OutputInterruptMessage::new(
                        interrupt_msg.payload.reason,
                        interrupt_msg.payload.source,
                    );
                    OutboundAction::Interrupt(handle.tx_interrupt.clone(), outbound)
                }
                InputEvent::Compress(_) => {
                    // 控制通道：手动压缩请求转化为 ControlCommand::Compress，送主循环 turn 边界消费
                    //（跳过阈值 / 反抖动，复用自动压缩执行流程，reason=manual）
                    OutboundAction::Control(
                        handle.tx_control.clone(),
                        fuyao_api::ControlCommand::Compress,
                    )
                }
                InputEvent::Rollback(req) => {
                    // 控制通道：对话回退请求转化为 ControlCommand::Rollback，送主循环 turn 边界消费。
                    // task 在 turn 边界自执行回退（删目标 seq 之后的消息 + 重算会话状态），
                    // 结果经 per-session 出口以 OutputEvent::Rollback 事件流出。
                    // 与 Compress 同构——控制通道是 fire-and-forget 载体，回执由事件出口承担。
                    OutboundAction::Control(
                        handle.tx_control.clone(),
                        fuyao_api::ControlCommand::Rollback {
                            target_seq: req.payload.target_seq,
                        },
                    )
                }
            }
        };

        // 锁外投递：三种通道的 send 语义一致（满则等待，断则 Shutdown），
        // 仅载荷类型不同，统一走泛型 deliver
        match action {
            OutboundAction::Inbound(tx, msg) => deliver(tx, msg).await?,
            OutboundAction::Interrupt(tx, msg) => deliver(tx, msg).await?,
            OutboundAction::Control(tx, cmd) => deliver(tx, cmd).await?,
        }

        Ok(())
    }

    /// 更新对话级参数（运行时调整 session 配置）
    ///
    /// 整 session 全程只有一份 `SessionParams`（存在 `SessionCtx` 的共享句柄），
    /// 本方法直接覆盖该句柄，消费点（跑 turn、压缩）下次现读即用新值——
    /// 不存在"生效时机"概念，普通"改一个可变变量"。
    ///
    /// 无论传入什么 `SessionParams`，整个直接替代当前值——agent_config / model_config
    /// 一视同仁全量覆盖，调用方给什么就用什么。
    ///
    /// TODO: agent_config 运行时切换的前缀缓存问题。system_prompt 在 create_session 时
    ///   已烘进 Session.system_prompt，此处改 agent_config.definition 后，新定义要到
    ///   下次压缩重建 system_prompt 时才会反映进提示词（react/mod.rs 的压缩分支）。
    ///   即 agent_config 切换不会立即重建提示词——「不破坏前缀缓存的提示词立即重建」
    ///   方案待设计（可能的方向：预热新前缀后切 / 增量提示词注入 / 强制开新 session）。
    ///   在那之前：model_config 立即生效无副作用；agent_config 切换的延迟生效行为见上。
    ///
    /// session id 不在调度表 → 同步返回 `Err(SessionNotFound)`。
    pub async fn update_session_params(
        &self,
        id: &SessionId,
        params: SessionParams,
    ) -> Result<(), EngineError> {
        // sessions 锁作用域内仅取出参数共享句柄，锁外再锁写——
        // 持调度表锁等待 session_params 锁会形成嵌套锁，session task
        // 锁参数期间全引擎 session 操作都被拖住
        let params_handle = {
            let sessions = self.sessions.lock().await;
            sessions
                .get(id)
                .map(|handle| Arc::clone(&handle.session_params))
                .ok_or_else(|| EngineError::SessionNotFound(id.clone()))?
        };
        // 全量直接替代：调用方给什么 SessionParams 就用什么，不做任何字段拦截。
        *params_handle.lock().await = params;
        tracing::info!(session_id = %id, "对话参数已更新");
        Ok(())
    }
}
