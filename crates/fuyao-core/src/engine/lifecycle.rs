//! 会话生命周期管理（创建 / 恢复 / 派生 / 子任务）
//!
//! 本模块集中 [`Engine`] 的「出生」相关动作：
//! - [`Engine::create_session`]：从零创建新 session
//! - [`Engine::resume_session`]：从数据库恢复老 session
//! - [`Engine::fork_session`]：派生独立 session（复制源可见上下文）
//! - [`Engine::create_child_session`]：创建带父标记的子任务 session（rx 直接交调用方，不进 fan-in）
//!
//! 以及这些动作共用的内部装配逻辑（[`Engine::assemble_session`] /
//! [`Engine::assemble_session_hooks`]）与 fork 拷贝核心
//! （[`Engine::build_forked_session`]）。
//!
//! 所有创建方法都返 `(SessionId, Receiver<OutputEvent>)`——id 用于后续 send/end，
//! rx 用于消费该 session 的产出事件（per-session 通道化）。

use super::*;
use crate::emit::Emitter;
use fuyao_api::AgentConfig;

impl Engine {
    /// 创建对话（动作二）
    ///
    /// 从零创建一个新 Session：构建系统提示词、生成编号、登记进调度表、落库。
    /// `SessionParams` 创建时定死且不可变（Agent 配置改了会冲掉前缀缓存）。
    ///
    /// 返回 `(session_id, rx)`——rx 是该 session 的 per-session 出站通道接收端，
    /// 调用方独占消费该 session 的所有 `OutputEvent`。
    pub async fn create_session(
        &self,
        params: SessionParams,
    ) -> Result<(SessionId, mpsc::UnboundedReceiver<OutputEvent>), EngineError> {
        // 加载完整 Agent 定义一次：同时供系统提示词构建与 per-session 工具过滤（避免重复加载）。
        // definition 必填，未知名 / mode 不符直接报错返回
        let usage = fuyao_prompt::PromptUsage::Primary;
        let definition = fuyao_prompt::resolve_definition(
            &self.params.agent_paths,
            &params.agent_config,
            usage,
        )?;
        // 构建系统提示词（Agent 配置决定人格）
        let system_prompt = build_system_prompt(&self.params.agent_paths, &definition, usage);

        // 创建 Session（8 位 id）。工作目录来自 agent_paths.workspace，创建时定死，
        // 经 normalize_workspace 统一分隔符为正斜杠（跨平台形态一致，按项目过滤匹配稳定）。
        let workspace = fuyao_api::normalize_workspace(&self.params.agent_paths.workspace);
        let mut session = Session::new(workspace, None, Some(system_prompt));

        // 落库元数据（消息产生时由 emit_to_history 单条 insert_message 落库）。
        // id 随机生成，主键冲突时由 create_with_retry 重新生成重试。
        self.store.create_with_retry(&mut session).await?;

        let session_id = session.id.clone();

        // 装配 session（建队列/通道 + 装配 hooks + spawn task + 登记）
        // SessionParams 整体传下去，不在入口拆包——压缩重建 prompt 等运行时场景
        // 仍需 agent_config，贯穿到 SessionCtx 留存，将来加字段只动 SessionCtx 一处
        // is_child 从新建 session 行的 parent_session_id 派生（新建恒为 None → false）
        let (handle, rx_event) = self.assemble_session(
            session_id.clone(),
            session.parent_session_id.is_some(),
            params,
            definition,
        );
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), handle);

        tracing::info!(session_id = %session_id, "创建对话");
        Ok((session_id, rx_event))
    }

    /// 恢复对话（动作三）
    ///
    /// 把数据库里的老对话捞回内存：用 session id 从 store 加载历史，
    /// 装进内存，重新登记进调度表。
    ///
    /// 与 `create_session` 对称，同样由调用方提供 `SessionParams`
    /// （含 Agent 配置）——引擎核心不持久化 AgentConfig，恢复时由
    /// 调用方把创建时的那份配置原样再传一次。
    ///
    /// 返回 `(session_id, rx)`——id 与传入相同，rx 是该 session 的 per-session 出站通道。
    ///
    /// session id 不在数据库 → 同步返回 `Err(SessionNotFound)`。
    pub async fn resume_session(
        &self,
        id: &SessionId,
        params: SessionParams,
    ) -> Result<(SessionId, mpsc::UnboundedReceiver<OutputEvent>), EngineError> {
        // 从数据库加载
        let session = self
            .store
            .get(id)
            .await?
            .ok_or_else(|| EngineError::SessionNotFound(id.clone()))?;

        // 加载完整 Agent 定义（创建时定死语义：恢复时按同一 agent_config 重新加载，
        // per-session 持有供工具过滤）。usage 按恢复 session 的 parent 判定。
        let is_child = session.parent_session_id.is_some();
        let usage = if is_child {
            fuyao_prompt::PromptUsage::Subagent
        } else {
            fuyao_prompt::PromptUsage::Primary
        };
        let definition = fuyao_prompt::resolve_definition(
            &self.params.agent_paths,
            &params.agent_config,
            usage,
        )?;

        // 装配 session（建队列/通道 + 装配 hooks + spawn task + 登记）
        // SessionParams 整体传下去（与 create_session 对称）
        let (handle, rx_event) = self.assemble_session(id.clone(), is_child, params, definition);
        self.sessions.lock().await.insert(id.clone(), handle);

        tracing::info!(session_id = %id, "恢复对话");
        Ok((id.clone(), rx_event))
    }

    /// 派生对话（fork：从源 session 复制可见上下文到新独立 session）
    ///
    /// 创建一个新 Session 作为源 session 的派生：复制源 session 的**系统提示词**与
    /// **可见消息**到新 session。新 session 是独立 session（**不是**子任务），
    /// 其 `parent_session_id` 保持 `None`——适合「分支对话探索」这类仍属主对话的形态。
    /// 新 session 装配完成后即可立即接收 `send` 跑 ReAct，与 `create_session` 产出的等价。
    ///
    /// 若要创建带父标记的子任务 session（后台任务 / 子代理），用
    /// [`create_child_session`](Self::create_child_session)。
    ///
    /// # 复制语义
    /// - **系统提示词**：取 sessions 表持久化的 `system_prompt` 原值，不从 `agent_config` 重建
    ///   （源 session 的提示词可能已被压缩重建过，重建值才是模型真正看到的）
    /// - **可见消息**：用 `load_visible_messages`（尊重 compaction 边界），**不用** `load_full_history`
    ///   （后者含压缩前旧消息 + 副本，仅审计用）。`insert_messages_batch` 单事务批量写入
    ///   新 session，自动分配新 seq（与压缩 `apply` 的 copy-to-new-seq 模式一致）
    /// - **派生归属**：`parent_session_id = None`（独立 session，非子任务）
    ///
    /// # SessionParams 处理
    /// 与 `resume_session` 对称——由调用方提供 `SessionParams`（`agent_config` + `model_config`）。
    /// `agent_config` 贯穿到 `SessionCtx` 供未来压缩重建提示词用；`model_config` 决定首轮用哪个模型。
    /// 注：本方法不重建 system_prompt（直接复制源的），故 `agent_config` 不影响 fork 时的初始提示词，
    /// 仅在 fork 出的 session 自身后续压缩时才参与重建。
    ///
    /// # 统计字段初值
    /// 新 session 的费用 / token 统计从 0 起算（派生会话自身的开销独立计量，不继承源 session 的花费）；
    /// `message_count` 设为复制的**普通消息**条数（排除 compaction 边界，与 `emit_to_history`
    /// 的计数语义一致——`mark_compaction` 不 bump 该计数）。
    ///
    /// # 返回
    /// `(new_session_id, rx)`——rx 是新 session 的 per-session 出站通道。
    ///
    /// # 错误
    /// - [`EngineError::SessionNotFound`]：源 session id 在数据库中不存在
    /// - [`EngineError::Storage`]：复制消息或落库失败
    /// - [`EngineError::Prompt`]：`agent_config.definition` 未知名，或 mode 与主 Agent 用途不符
    pub async fn fork_session(
        &self,
        source_id: &SessionId,
        params: SessionParams,
    ) -> Result<(SessionId, mpsc::UnboundedReceiver<OutputEvent>), EngineError> {
        // 纯 fork：复制源可见上下文，parent=None（fork 出的是独立 session，非子任务）
        let new_session = self.build_forked_session(source_id, None).await?;
        let new_session_id = new_session.id.clone();

        // 加载完整 Agent 定义（fork 出的独立 session parent=None → 主 Agent 用途）
        let definition = fuyao_prompt::resolve_definition(
            &self.params.agent_paths,
            &params.agent_config,
            fuyao_prompt::PromptUsage::Primary,
        )?;

        // 装配 session（队列 / 通道 / hooks / task）+ 登记进调度表
        // SessionParams 整体传下去（与 create_session / resume_session 对称）
        // fork 出的是独立 session（parent=None）→ is_child=false
        let (handle, rx_event) =
            self.assemble_session(new_session_id.clone(), false, params, definition);
        self.sessions
            .lock()
            .await
            .insert(new_session_id.clone(), handle);

        tracing::info!(
            session_id = %new_session_id,
            source_session_id = %source_id,
            "派生对话已创建（独立 session，parent=None）"
        );
        Ok((new_session_id, rx_event))
    }

    /// 创建子任务 session（后台任务 / 子代理地基）
    ///
    /// 产出一个带 `parent_session_id` 的子任务 session，经 `assemble_session` 装配 +
    /// 登记调度表后可立即接收 `send` 跑 ReAct。与 [`create_session`](Self::create_session)
    /// 的唯一差异是 `parent_session_id` 标记：主 session 为 `None`，子任务为 `Some(父 id)`，
    /// 供前端区分主对话 vs 子任务并按父 id 分组/过滤。
    ///
    /// 无论子任务是全新创建还是 fork 旧 session，只要它是子任务就带 `parent_session_id`。
    ///
    /// # 两种上下文模式（[`ChildSessionSource`]）
    /// - [`Fresh`](ChildSessionSource::Fresh)：空上下文，`system_prompt` 从 `agent_config` 构建
    ///   （与 `create_session` 一致）
    /// - [`Fork`](ChildSessionSource::Fork)：复制某源 session 的可见消息 + `system_prompt`
    ///   （复用 [`fork_session`](Self::fork_session) 的拷贝逻辑）
    ///
    /// # 参数处理
    /// 调用方只提供子代理的 `agent_config`（definition）；`model_config` 由引擎从父 session
    /// 继承——子代理复用父模型，无需调用方重复指定。整体贯穿到 `SessionCtx`。
    /// Fresh 模式用 `agent_config` 构建初始 `system_prompt`；Fork 模式不重建（直接复制源的），
    /// `agent_config` 仅在子任务 session 后续压缩时参与重建。
    ///
    /// # 返回
    /// `(session_id, rx)`——rx 是子任务 session 的 per-session 出站通道，由调用方独占消费。
    /// 子 session 不进 fan-out（UI 出口只暴露主对话）；调用方按业务决定如何消费 rx：
    /// - 同步子代理 tool handler：消费 rx 取 `finish_reason=stop` 的最终回复回喂父 ReAct
    /// - fire-and-forget 后台任务：spawn 独立 task 消费 rx（写日志 / 丢弃均可——事件已落库）
    ///
    /// # 错误
    /// - [`EngineError::SessionNotFound`]：父 session 不在调度表（未创建 / 已结束），或 `Fork` 模式的源 session id 在数据库中不存在
    /// - [`EngineError::Storage`]：落库失败
    /// - [`EngineError::Prompt`]：`child_agent_config.definition` 未知名，或 mode 与子代理用途不符
    pub async fn create_child_session(
        &self,
        parent_session_id: &SessionId,
        source: ChildSessionSource,
        child_agent_config: AgentConfig,
    ) -> Result<(SessionId, mpsc::UnboundedReceiver<OutputEvent>), EngineError> {
        // 子会话复用父会话的 model_config（子代理用父模型）；agent_config 用子代理自己的 definition。
        // 从父 session 调度表读 model_config——父派生子代理时一定活跃（正跑 turn）。
        // sessions 锁在 block 内先 drop，再 await session_params 锁，避免持锁跨 await（铁律）。
        let model_config = {
            let parent_params = {
                let sessions = self.sessions.lock().await;
                sessions
                    .get(parent_session_id)
                    .map(|h| Arc::clone(&h.session_params))
                    .ok_or_else(|| EngineError::SessionNotFound(parent_session_id.clone()))?
            };
            parent_params.lock().await.model_config.clone()
        };
        let params = SessionParams {
            agent_config: child_agent_config,
            model_config,
        };
        // 子任务 session 固定 Subagent 用途（parent_session_id = Some），definition 加载一次
        // 供 Fresh 模式构建系统提示词 + assemble per-session 工具过滤复用
        let usage = fuyao_prompt::PromptUsage::Subagent;
        // 先按模式构造 + 落库新 session（不带 assemble，assemble 在统一出口做）
        let (new_session, definition) = match source {
            ChildSessionSource::Fresh => {
                // 全新子任务：空上下文，definition 加载后构建 system_prompt
                let definition = fuyao_prompt::resolve_definition(
                    &self.params.agent_paths,
                    &params.agent_config,
                    usage,
                )?;
                let system_prompt =
                    build_system_prompt(&self.params.agent_paths, &definition, usage);
                let workspace = fuyao_api::normalize_workspace(&self.params.agent_paths.workspace);
                let mut s = Session::new(workspace, None, Some(system_prompt));
                s.parent_session_id = Some(parent_session_id.clone());
                self.store.create_with_retry(&mut s).await?;
                (s, definition)
            }
            ChildSessionSource::Fork(ref source_id) => {
                // fork 子任务：复制源可见上下文，parent 标记为父 session id
                let s = self
                    .build_forked_session(source_id, Some(parent_session_id.clone()))
                    .await?;
                let definition = fuyao_prompt::resolve_definition(
                    &self.params.agent_paths,
                    &params.agent_config,
                    usage,
                )?;
                (s, definition)
            }
        };

        let new_session_id = new_session.id.clone();

        // 装配 session（队列 / 通道 / hooks / task）+ 登记进调度表（与 create_session 对称）
        // create_child_session 产出的恒为子任务（parent_session_id 非空）→ is_child=true
        let (handle, rx_event) =
            self.assemble_session(new_session_id.clone(), true, params, definition);
        self.sessions
            .lock()
            .await
            .insert(new_session_id.clone(), handle);

        tracing::info!(
            session_id = %new_session_id,
            parent_session_id = %parent_session_id,
            "子任务 session 已创建"
        );
        Ok((new_session_id, rx_event))
    }

    /// 从源 session 复制可见消息 + system_prompt，构造并落库新 session（fork 拷贝核心）
    ///
    /// 共享给 [`fork_session`](Self::fork_session)（`parent_session_id = None`，纯 fork）
    /// 与 [`create_child_session`](Self::create_child_session) 的 `Fork` 模式
    /// （`parent_session_id = Some(父 id)`，子任务 fork）。仅做 DB 层的加载 + 复制 + 落库，
    /// **不** assemble / **不**登记调度表——由调用方完成 `assemble_session` 与注册。
    ///
    /// # 复制语义
    /// - **system_prompt**：取源 session 持久化原值，不从 `agent_config` 重建
    /// - **可见消息**：用 `load_visible_messages`（尊重 compaction 边界），
    ///   `insert_messages_batch` 单事务批量写入新 session，seq 连续重新分配
    ///   （与压缩 `apply` 的 copy-to-new-seq 模式一致；消息复制整批原子，
    ///   全部成功或全部回滚）
    /// - **message_count**：对齐复制的**普通消息**条数（排除 compaction 边界，
    ///   与 `emit_to_history` 计数语义一致）
    /// - **统计字段**（token / 费用）：从 0 起算，不继承源 session
    ///
    /// # 错误
    /// - [`EngineError::SessionNotFound`]：源 session id 在数据库中不存在
    /// - [`EngineError::Storage`]：复制消息或落库失败。新 session 元数据行是独立事务
    ///   先落库，复制阶段整批原子回滚——失败时仅剩一条无消息的孤立 session 行
    ///   （未装配登记、不进调度表，不影响引擎调度）
    pub(super) async fn build_forked_session(
        &self,
        source_id: &SessionId,
        parent_session_id: Option<String>,
    ) -> Result<Session, EngineError> {
        // 1. 加载源 session（取持久化的 system_prompt + 校验存在）
        let source = self
            .store
            .get(source_id)
            .await?
            .ok_or_else(|| EngineError::SessionNotFound(source_id.clone()))?;

        // 2. 加载源 session 的可见消息（可见窗口 = 最新 compaction 摘要 + 摘要后全部消息），
        //    fork 完整复制源会话可见窗口——子会话继承压缩边界之后的上下文
        let visible = self.store.load_visible_messages(source_id).await?;

        // 3. 构造新 session：系统提示词复制源值，parent_session_id 由调用方决定
        //    计数字段（message_count 等）从 0 起算——下方批量复制消息时，
        //    事务内会按消息 kind/role 一次性聚合累加（普通消息 +1 message_count，role=Tool +1 tool_call_count），
        //    复制完成后 DB 里的计数值自然对齐复制的消息条数。
        let mut new_session =
            Session::new(source.workspace.clone(), None, source.system_prompt.clone());
        new_session.parent_session_id = parent_session_id;

        // 4. 落库新 session 元数据行（先建行，满足 messages.session_id 外键约束）。
        //    id 随机生成，主键冲突时由 create_with_retry 重新生成重试。
        self.store.create_with_retry(&mut new_session).await?;

        // 5. 批量复制可见消息到新 session：visible 所有权直接移交批量落库
        //    （seq 由批量方法统一重新分配，覆盖源 session 带来的旧值，无需逐条 clone 重置）
        //    与压缩 apply 的 copy-to-new-seq 模式一致：整批单事务原子，全部成功或全部回滚，
        //    失败即返回 Err（此时新 session 元数据行已先落库但未装配登记，为无消息的孤立行，
        //    不影响引擎调度）
        //    批量落库事务内一次性聚合累加 sessions 计数——复制完全等于"重新产生这些消息"
        let mut copies = visible;
        self.store
            .insert_messages_batch(&new_session.id, &mut copies)
            .await?;

        Ok(new_session)
    }

    /// 装配 session（create_session / resume_session 公共方法）
    ///
    /// 建该 session 专属的双队列 + 三条通道（inbound/interrupt/control + per-session 出站），
    /// 装配该 session 的 hooks（per-session 独立实例），spawn 执行流 task，
    /// 返回 `(SessionHandle, rx_event)`——rx_event 是该 session 的独立出站通道接收端，
    /// 由调用方（装配层 / detached 调用方）独占消费。
    fn assemble_session(
        &self,
        session_id: SessionId,
        is_child: bool,
        session_params: SessionParams,
        definition: fuyao_api::AgentDefinition,
    ) -> (SessionHandle, mpsc::UnboundedReceiver<OutputEvent>) {
        // 双队列
        let guide: SharedQueue = Arc::new(StdMutex::new(std::collections::VecDeque::new()));
        let pending: SharedQueue = Arc::new(StdMutex::new(std::collections::VecDeque::new()));

        // SessionParams 共享句柄：整 session 全程只有一份，Engine 可写（update_session_params）、
        // task 现读现用（跑 turn、压缩取 model_config）。一份 clone 给 SessionCtx，一份留 Handle。
        // 用 tokio::Mutex：Engine 写与 task 读均跨 async 上下文（与 SessionCtx.session_params 同型）。
        let session_params = Arc::new(Mutex::new(session_params));

        // 定义层未知名对账（与全局 [tools.enabled] 同款：静默忽略 + WARN，用户错误用户承担）。
        // definition 已由调用方加载（resolve_definition），此处仅留痕。
        for name in fuyao_api::unknown_tool_names(&definition.tools, self.tools.names()) {
            tracing::warn!(
                tool_name = %name,
                layer = "definition",
                "工具配置引用了未知的工具名，已忽略"
            );
        }

        // 三条 session 级入站通道（载荷统一为 output 侧类型——入口转化后内核只认 output 侧）
        let (tx_inbound, rx_inbound) = mpsc::channel::<fuyao_api::message::output::UserMessage>(16);
        let (tx_interrupt, rx_interrupt) = mpsc::channel::<OutputInterruptMessage>(8);
        // 控制通道：承载 ControlCommand（手动压缩等 B 类信号），主循环 turn 边界消费。
        // 容量小（8）——B 类命令频率低，主循环串行消费天然去重。
        let (tx_control, rx_control) = mpsc::channel::<fuyao_api::ControlCommand>(8);

        // 该 session 的 per-session 出站通道（无界——事件入 channel 前已落库，
        // 不让 emit 阻塞反压到 ReAct turn 推进）
        let (tx_event, rx_event) = mpsc::unbounded_channel::<OutputEvent>();

        // turn 相位通道：task 侧 sender 进 SessionCtx（进 turn 区间置 Running / 退出回 Idle），
        // Engine 侧 receiver 进 SessionHandle（stop_session 屏障等待用）
        let (turn_phase_tx, turn_phase_rx) =
            tokio::sync::watch::channel(crate::engine::types::TurnPhase::Idle);

        // 该 session 的关闭信号（引擎级 shutdown_token 的 child_token）
        //   Engine::shutdown 调 root.cancel → 所有 child 同时 cancel
        //   未来扩展「单 session 销毁」时可单独 cancel 这个 child
        let shutdown_token = self.shutdown_token.child_token();

        // 装配该 session 的 hooks（per-session：create_instances + register + finalize）
        // 实例集合随 hooks 一起产出，存进 SessionHandle 供 session 结束时逆序 dispose
        let (hooks, plugin_instances) =
            self.assemble_session_hooks(&session_id, tx_inbound.clone(), tx_interrupt.clone());

        // 装配 SessionCtx（会话级共享依赖的 owned 视图）+ SessionRx（入站通道集合）。
        // SessionCtx 统一经 builder 构造（与测试共用唯一构造点）：必填字段位置参数钉死，
        // 可选字段生产侧全部显式覆盖——不用默认值，漏配即显式可见（fail-loud）。
        let ctx = react::SessionCtx::builder(
            Arc::clone(&self.store),
            Arc::clone(&self.providers),
            Arc::clone(&self.tools),
            hooks,
            self.params.agent_paths.clone(),
            definition,
            Arc::clone(&session_params),
            Emitter::new(tx_event, session_id.clone()),
            is_child,
        )
        .guide(Arc::clone(&guide))
        .pending(Arc::clone(&pending))
        .compression_config(fuyao_api::get_config().session.compression.clone())
        .shutdown_token(shutdown_token.clone())
        .turn_phase(turn_phase_tx)
        .subagent_ops(Some(self.subagent_ops_weak()))
        .build();
        let rx = react::SessionRx {
            inbound: rx_inbound,
            interrupt: rx_interrupt,
            control: rx_control,
        };
        let task = tokio::spawn(react::run_session(ctx, rx));

        (
            SessionHandle {
                guide,
                pending,
                tx_inbound,
                tx_interrupt,
                tx_control,
                turn_phase_rx,
                task,
                shutdown_token,
                session_params,
                plugin_instances,
            },
            rx_event,
        )
    }

    /// 装配某 session 的 hooks（per-session，每 session 调用一次）
    ///
    /// 流程：
    /// 1. `plugin_host.create_instances()` 生成该 session 的所有 `(插件名, 实例)` 配对
    ///    （含重名检查 + create_instance panic 防护）
    /// 2. 每个 `instance.register(&mut registry, &sender)` 注册到该 session 私有的
    ///    registry（sender 绑该插件名，register panic 单独防护）
    /// 3. `registry.finalize()` 排定优先级，冻结后包 `Arc` 只读共享
    /// 4. 实例集合随 hooks 一起返回——`SessionHandle` 持有，session 结束时逆序 dispose
    ///    （生命周期闭环：注册进 registry 的闭包只持弱引用视角，实例本体的销毁义务
    ///    由 handle 承载）
    ///
    /// 返回 `(共享 hooks, 插件实例集合)`。
    ///
    /// 失败容错：插件实例化失败（重名等）该 session 以**空 hooks** 运行（不硬 panic，让 session 还能用），
    /// 此时实例集合也为空（无实例可 dispose）。
    fn assemble_session_hooks(
        &self,
        session_id: &SessionId,
        tx_inbound: mpsc::Sender<fuyao_api::message::output::UserMessage>,
        tx_interrupt: mpsc::Sender<OutputInterruptMessage>,
    ) -> (SharedHooks, Vec<NamedPluginInstance>) {
        let mut registry = HooksRegistry::new();

        // 1. 工厂生产实例（同步，host 内部已含 create_instance panic 防护）
        let instances = match self.plugin_host.create_instances() {
            Ok(insts) => insts,
            Err(e) => {
                tracing::error!(
                    session_id = %session_id,
                    cause = %e,
                    "插件实例化失败（重名或装配错误），该 session 将以空 hooks 运行"
                );
                return (Arc::new(registry), Vec::new());
            }
        };

        // 2. 每个 instance 注册 hook（register 是同步调用，单独 panic 防护）
        //    每个实例拿到绑定自己插件名的 sender（注入消息 source 可追溯）
        //    单个 instance.register panic 不阻塞其他实例注册
        for (name, instance) in &instances {
            let sender = SessionSender::new(name.clone(), tx_inbound.clone(), tx_interrupt.clone());
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                instance.register(&mut registry, &sender)
            }));
            if let Err(payload) = result {
                tracing::warn!(
                    session_id = %session_id,
                    plugin = %name,
                    phase = "register",
                    recovered = true,
                    cause = %fuyao_hooks::panic_payload_to_string(&*payload),
                    "插件实例 register panic 已恢复"
                );
            }
        }

        // 3. 排定优先级后冻结，包 Arc 只读共享（运行期无锁）
        registry.finalize();

        // 4. hooks 与实例集合一起交还：hooks 进 SessionCtx 运行，实例进 SessionHandle 待 dispose
        (Arc::new(registry), instances)
    }
}
