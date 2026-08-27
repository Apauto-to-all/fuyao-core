//! 会话运行时操作（入站分发 / 参数更新 / 队列删除）
//!
//! 本模块集中 [`Engine`] 的「运行」相关动作：
//! - [`Engine::send`]：入站事件单一入口（User / Control / Interrupt 分流）
//! - [`Engine::update_session_params`]：运行时调整 session 参数
//! - [`Engine::remove_queued_message`]：按客户端消息标识删除双队列中未消费的条目
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
    /// User / Control 消息条目 → 入站通道
    Inbound(mpsc::Sender<QueueEntry>, QueueEntry),
    /// 中断信号 → 中断通道
    Interrupt(mpsc::Sender<OutputInterruptMessage>, OutputInterruptMessage),
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
    /// - `Control`：控制命令消息，与用户消息同走入站通道（保证总序），进双队列后
    ///   在消费时机执行；mode 决定生效时机（Guide = 下一消费时机 / Pending = 最终回复后），
    ///   结果经 per-session 出口以对应 OutputEvent 流出（如 Compression）
    /// - `Interrupt`：发出中断信号，打断对应对话的当前执行
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
                    // （base + payload 完整保留，含 source），包成 User 条目送进 session task。
                    // 模型/思考等运行时配置挂在 session 级（SessionParams.model_config），
                    // 消费点（跑 turn、压缩）现读现用，不随消息携带。
                    // 后续 handle_inbound_item 纯入队，inject_user_messages 消费时统一过管道。
                    let outbound = OutputUserMessage {
                        base: user_msg.base,
                        payload: fuyao_api::message::output::UserPayload {
                            content: user_msg.payload.content,
                            images: user_msg.payload.images,
                            mode: user_msg.payload.mode,
                            source: user_msg.payload.source,
                            client_message_id: user_msg.payload.client_message_id,
                        },
                    };
                    OutboundAction::Inbound(handle.tx_inbound.clone(), QueueEntry::User(outbound))
                }
                InputEvent::Control(ctrl_msg) => {
                    // input 侧 ControlMessage 字段照搬转化为 output 侧 ControlMessage
                    // （command / mode / client_message_id），包成 Control 条目走与用户消息
                    // 相同的入站通道——同一通道承载保证两类消息的总序。
                    // 命令本体（如手动压缩跳过阈值，reason=manual）由消费点的
                    // handle_control 先回显后执行，client_message_id 供排队中
                    // 撤销与消费回显配对。
                    let outbound = fuyao_api::message::output::ControlMessage {
                        base: ctrl_msg.base,
                        payload: fuyao_api::message::output::ControlPayload {
                            command: ctrl_msg.payload.command,
                            mode: ctrl_msg.payload.mode,
                            client_message_id: ctrl_msg.payload.client_message_id,
                        },
                    };
                    OutboundAction::Inbound(
                        handle.tx_inbound.clone(),
                        QueueEntry::Control(outbound),
                    )
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
            }
        };

        // 锁外投递：两类通道的 send 语义一致（满则等待，断则 Shutdown），
        // 仅载荷类型不同，统一走泛型 deliver
        match action {
            OutboundAction::Inbound(tx, entry) => deliver(tx, entry).await?,
            OutboundAction::Interrupt(tx, msg) => deliver(tx, msg).await?,
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

    /// 按客户端消息标识删除双队列中未消费的条目
    ///
    /// 依次在 pending、guide 两队列中移除所有 `client_message_id` 匹配的条目
    /// （User 与 Control 条目按各自 `payload.client_message_id` 匹配——排队中
    /// 尚未生效的命令同样可撤销），返回删除条数（0 = 两队列中均无此标识的条目）。
    /// 只删**未消费**的条目：已被 turn 消费（drain 出队、注入历史或执行命令）的
    /// 条目不在此方法管辖内。
    ///
    /// 旁路管理方法：不经过 session 通道、不触发任何钩子或事件——删除是纯内存
    /// 队列操作，session task 与调用方对同一队列各持 `Arc`，短临界区天然互斥。
    ///
    /// `client_message_id` 由发送方生成、会话内唯一，引擎信任不校验；标识仅存活
    /// 于队列流转与消费回显（供前端配对），一律不落库。
    ///
    /// session id 不在调度表 → 同步返回 `Err(SessionNotFound)`（要恢复走恢复动作）。
    pub async fn remove_queued_message(
        &self,
        id: &SessionId,
        client_message_id: &str,
    ) -> Result<usize, EngineError> {
        // shutdown 同步快路径检查：已关闭立即拒绝（区分于 SessionNotFound）
        if self.shutdown.load(Ordering::Acquire) {
            return Err(EngineError::Shutdown);
        }

        // 调度表锁作用域内仅取出双队列共享句柄（clone 廉价）即释放锁——
        // 队列操作在锁外进行，与 send 同款锁纪律
        let (pending, guide) = {
            let sessions = self.sessions.lock().await;
            let handle = sessions
                .get(id)
                .ok_or_else(|| EngineError::SessionNotFound(id.clone()))?;
            (Arc::clone(&handle.pending), Arc::clone(&handle.guide))
        };

        // 依次锁 pending、锁 guide（std Mutex，锁内仅 retain 纯内存操作、无 await）；
        // 两把锁不同时持有，无锁序问题
        let removed_pending = retain_out_matching(&pending, client_message_id);
        let removed_guide = retain_out_matching(&guide, client_message_id);
        let removed = removed_pending + removed_guide;

        tracing::info!(
            session_id = %id,
            client_message_id = client_message_id,
            removed,
            "队列消息已删除"
        );
        Ok(removed)
    }
}

/// 按客户端消息标识从单个队列移除匹配条目，返回移除条数
///
/// User 与 Control 两类条目都按各自 `payload.client_message_id` 匹配。
/// 锁内只做 retain（纯内存、无 await）；锁中毒时取回内部数据继续操作——
/// 队列数据本身完好，中毒不代表队列不可用。无标识（None）的条目
/// （系统 / 插件注入消息）永不匹配任何客户端标识。
fn retain_out_matching(queue: &SharedQueue, client_message_id: &str) -> usize {
    let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
    let before = q.len();
    q.retain(|entry| {
        let id = match entry {
            QueueEntry::User(m) => m.payload.client_message_id.as_deref(),
            QueueEntry::Control(m) => m.payload.client_message_id.as_deref(),
        };
        id != Some(client_message_id)
    });
    before - q.len()
}
