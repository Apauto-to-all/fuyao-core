//! 会话生命周期管理（创建 / 恢复 / 派生 / 子任务）
//!
//! 本模块集中 [`Engine`] 的「出生」相关动作：
//! - [`Engine::create_session`]：从零创建新 session
//! - [`Engine::resume_session`]：从数据库恢复老 session
//! - [`Engine::fork_session`]：派生独立 session（复制源可见上下文）
//! - [`Engine::create_child_session`]：创建带父标记的子任务 session
//!
//! 以及这些动作共用的内部装配逻辑（[`Engine::assemble_session`] /
//! [`Engine::assemble_session_hooks`]）与 fork 拷贝核心
//! （[`Engine::build_forked_session`]）。

use super::*;

impl Engine {
    /// 创建对话（动作二）
    ///
    /// 从零创建一个新 Session：构建系统提示词、生成编号、登记进调度表、落库。
    /// `SessionParams` 创建时定死且不可变（Agent 配置改了会冲掉前缀缓存）。
    ///
    /// 返回新 session id。
    pub async fn create_session(&self, params: SessionParams) -> Result<SessionId, EngineError> {
        // 构建系统提示词（Agent 配置决定人格）
        let system_prompt = build_system_prompt(&self.params.agent_paths, &params.agent_config);

        // 创建 Session（8 位 UUID）
        let session = Session::new(None, Some(system_prompt));

        // 落库元数据（消息产生时由 emit_to_history 单条 insert_message 落库）
        self.store.create(&session).await?;

        let session_id = session.id.clone();

        // 装配 session（建队列/通道 + 装配 hooks + spawn task + 登记）
        // SessionParams 整体传下去，不在入口拆包——压缩重建 prompt 等运行时场景
        // 仍需 agent_config，贯穿到 SessionCtx 留存，将来加字段只动 SessionCtx 一处
        let handle = self
            .assemble_session(session_id.clone(), session, params)
            .await;
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), handle);

        tracing::info!(session_id = %session_id, "创建对话");
        Ok(session_id)
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
    /// session id 不在数据库 → 同步返回 `Err(SessionNotFound)`。
    pub async fn resume_session(
        &self,
        id: &SessionId,
        params: SessionParams,
    ) -> Result<(), EngineError> {
        // 从数据库加载
        let session = self
            .store
            .get(id)
            .await?
            .ok_or_else(|| EngineError::SessionNotFound(id.clone()))?;

        // 装配 session（建队列/通道 + 装配 hooks + spawn task + 登记）
        // SessionParams 整体传下去（与 create_session 对称）
        let handle = self.assemble_session(id.clone(), session, params).await;
        self.sessions.lock().await.insert(id.clone(), handle);

        tracing::info!(session_id = %id, "恢复对话");
        Ok(())
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
    ///   （后者含压缩前旧消息 + 副本，仅审计用）。逐条 `insert_message` 写入新 session，
    ///   自动分配新 seq（与压缩 `apply` 的 copy-to-new-seq 模式一致）
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
    /// `message_count` 设为复制的**普通消息**条数（排除 compaction 边界，与 `emit_to_history` /
    /// `count_messages` 的计数语义一致——`mark_compaction` 不 bump 该计数）。
    ///
    /// # 错误
    /// - [`EngineError::SessionNotFound`]：源 session id 在数据库中不存在
    /// - [`EngineError::Storage`]：复制消息或落库失败
    pub async fn fork_session(
        &self,
        source_id: &SessionId,
        params: SessionParams,
    ) -> Result<SessionId, EngineError> {
        // 纯 fork：复制源可见上下文，parent=None（fork 出的是独立 session，非子任务）
        let new_session = self.build_forked_session(source_id, None).await?;
        let new_session_id = new_session.id.clone();

        // 装配 session（队列 / 通道 / hooks / task）+ 登记进调度表
        // SessionParams 整体传下去（与 create_session / resume_session 对称）
        let handle = self
            .assemble_session(new_session_id.clone(), new_session, params)
            .await;
        self.sessions
            .lock()
            .await
            .insert(new_session_id.clone(), handle);

        tracing::info!(
            session_id = %new_session_id,
            source_session_id = %source_id,
            "派生对话已创建（独立 session，parent=None）"
        );
        Ok(new_session_id)
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
    /// # SessionParams 处理
    /// 由调用方提供 `SessionParams`（`agent_config` + `model_config`），整体贯穿到 `SessionCtx`。
    /// Fresh 模式用 `agent_config` 构建初始 `system_prompt`；Fork 模式不重建（直接复制源的），
    /// `agent_config` 仅在子任务 session 后续压缩时参与重建。
    ///
    /// # 错误
    /// - [`EngineError::SessionNotFound`]：`Fork` 模式的源 session id 在数据库中不存在
    /// - [`EngineError::Storage`]：落库失败
    pub async fn create_child_session(
        &self,
        parent_session_id: &SessionId,
        source: ChildSessionSource,
        params: SessionParams,
    ) -> Result<SessionId, EngineError> {
        // 先按模式构造 + 落库新 session（不带 assemble，assemble 在统一出口做）
        let new_session = match source {
            ChildSessionSource::Fresh => {
                // 全新子任务：空上下文，system_prompt 从 agent_config 构建（与 create_session 一致）
                let system_prompt =
                    build_system_prompt(&self.params.agent_paths, &params.agent_config);
                let mut s = Session::new(None, Some(system_prompt));
                s.parent_session_id = Some(parent_session_id.clone());
                self.store.create(&s).await?;
                s
            }
            ChildSessionSource::Fork(ref source_id) => {
                // fork 子任务：复制源可见上下文，parent 标记为父 session id
                self.build_forked_session(source_id, Some(parent_session_id.clone()))
                    .await?
            }
        };

        let new_session_id = new_session.id.clone();

        // 装配 session（队列 / 通道 / hooks / task）+ 登记进调度表（与 create_session 对称）
        let handle = self
            .assemble_session(new_session_id.clone(), new_session, params)
            .await;
        self.sessions
            .lock()
            .await
            .insert(new_session_id.clone(), handle);

        tracing::info!(
            session_id = %new_session_id,
            parent_session_id = %parent_session_id,
            "子任务 session 已创建"
        );
        Ok(new_session_id)
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
    /// - **可见消息**：用 `load_visible_messages`（尊重 compaction 边界），逐条 `insert_message`
    ///   写入新 session，自动分配新 seq（与压缩 `apply` 的 copy-to-new-seq 模式一致）
    /// - **message_count**：对齐复制的**普通消息**条数（排除 compaction 边界，
    ///   与 `emit_to_history` / `count_messages` 计数语义一致）
    /// - **统计字段**（token / 费用）：从 0 起算，不继承源 session
    ///
    /// # 错误
    /// - [`EngineError::SessionNotFound`]：源 session id 在数据库中不存在
    /// - [`EngineError::Storage`]：复制消息或落库失败（此时新 session 行未落库，DB 无残留）
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

        // 2. 加载源 session 的可见消息（尊重 compaction 边界，不含压缩前旧消息）
        let visible = self.store.load_visible_messages(source_id).await?;

        // 3. 构造新 session：系统提示词复制源值，parent_session_id 由调用方决定
        //    message_count 对齐复制的**普通消息**条数（排除 compaction 边界，
        //    与 emit_to_history / count_messages 的计数语义一致——mark_compaction 不 bump 该计数）
        let message_count = visible
            .iter()
            .filter(|m| matches!(m.kind, MessageKind::Message))
            .count() as i64;
        let mut new_session = Session::new(None, source.system_prompt.clone());
        new_session.parent_session_id = parent_session_id;
        new_session.message_count = message_count;

        // 4. 落库新 session 元数据行（先建行，满足 messages.session_id 外键约束）
        self.store.create(&new_session).await?;

        // 5. 逐条复制可见消息到新 session（clone 后强制 seq=0，insert_message 分配新 seq）
        //    与压缩 apply 的 copy-to-new-seq 模式一致：不包外层事务，单条失败即返回 Err
        //    （此时新 session 行已落库但未装配登记，为 DB 中的孤立行，不影响引擎调度）
        for msg in &visible {
            let mut clone = msg.clone();
            clone.seq = 0;
            self.store
                .insert_message(&new_session.id, &mut clone)
                .await?;
        }

        Ok(new_session)
    }

    /// 装配 session（create_session / resume_session 公共方法）
    ///
    /// 建该 session 专属的双队列 + 四条通道（inbound/interrupt/plugin + 事件出口），
    /// 装配该 session 的 hooks（per-session 独立实例），spawn 执行流 task，
    /// 返回 SessionHandle 由调用方登记进调度表。
    ///
    /// 关键：`assemble_session_hooks` 必须 async（`init_send_inputs` 是 async），
    /// 故本方法也是 async。
    async fn assemble_session(
        &self,
        session_id: SessionId,
        session: Session,
        session_params: SessionParams,
    ) -> SessionHandle {
        // 双队列
        let guide: SharedQueue = Arc::new(StdMutex::new(std::collections::VecDeque::new()));
        let pending: SharedQueue = Arc::new(StdMutex::new(std::collections::VecDeque::new()));

        // SessionParams 共享句柄：整 session 全程只有一份，Engine 可写（update_session_params）、
        // task 现读现用（跑 turn、压缩取 model_config）。一份 clone 给 SessionCtx，一份留 Handle。
        // 用 tokio::Mutex：Engine 写与 task 读均跨 async 上下文（与 SessionCtx.session_params 同型）。
        let session_params = Arc::new(Mutex::new(session_params));

        // 三条 session 级通道（载荷统一为 output 侧类型——入口转化后内核只认 output 侧）
        let (tx_inbound, rx_inbound) = mpsc::channel::<fuyao_api::message::output::UserMessage>(16);
        let (tx_interrupt, rx_interrupt) = mpsc::channel::<OutputInterruptMessage>(8);
        let (tx_plugin, rx_plugin) = mpsc::channel::<OutputPluginMessage>(16);

        // 该 session 的关闭信号（引擎级 shutdown_token 的 child_token）
        //   Engine::shutdown 调 root.cancel → 所有 child 同时 cancel
        //   未来扩展「单 session 销毁」时可单独 cancel 这个 child
        let shutdown_token = self.shutdown_token.child_token();

        // 装配该 session 的 hooks（per-session：create_instances + register + SessionSender）
        let hooks = self
            .assemble_session_hooks(
                &session_id,
                tx_inbound.clone(),
                tx_interrupt.clone(),
                tx_plugin.clone(),
            )
            .await;

        // spawn 执行流 task（多传 rx_plugin 参数）
        let task = tokio::spawn(react::run_session(
            session_id.clone(),
            Arc::clone(&guide),
            Arc::clone(&pending),
            rx_inbound,
            rx_interrupt,
            rx_plugin,
            shutdown_token.clone(),
            session,
            Arc::clone(&self.store),
            Arc::clone(&self.providers),
            Arc::clone(&self.tools),
            hooks,
            self.params.agent_paths.clone(),
            Arc::clone(&session_params),
            self.tx_event.clone(),
        ));

        SessionHandle {
            guide,
            pending,
            tx_inbound,
            tx_interrupt,
            tx_plugin,
            task,
            shutdown_token,
            session_params,
        }
    }

    /// 装配某 session 的 hooks（per-session，每 session 调用一次）
    ///
    /// 流程：
    /// 1. `plugin_host.create_instances()` 生成该 session 的所有插件实例（含重名检查 + create_instance panic 防护）
    /// 2. 每个 `instance.register(&mut registry)` 注册到该 session 私有的 registry（register panic 单独防护）
    /// 3. 构造 `SessionSender`（绑定该 session 的三条通道）
    /// 4. `registry.init_send_inputs(sender).await` 把 sender 传给 send_input hook
    /// 5. 包成 `SharedHooks` 返回
    ///
    /// 失败容错：插件实例化失败（重名等）该 session 以**空 hooks** 运行（不硬 panic，让 session 还能用）。
    async fn assemble_session_hooks(
        &self,
        session_id: &SessionId,
        tx_inbound: mpsc::Sender<fuyao_api::message::output::UserMessage>,
        tx_interrupt: mpsc::Sender<OutputInterruptMessage>,
        tx_plugin: mpsc::Sender<OutputPluginMessage>,
    ) -> SharedHooks {
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
                return Arc::new(Mutex::new(registry));
            }
        };

        // 2. 每个 instance 注册 hook（register 是同步调用，单独 panic 防护）
        //    单个 instance.register panic 不阻塞其他实例注册
        for (idx, instance) in instances.iter().enumerate() {
            let hint = format!("instance-{idx}");
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                instance.register(&mut registry)
            }));
            if let Err(payload) = result {
                tracing::warn!(
                    session_id = %session_id,
                    hint = %hint,
                    phase = "register",
                    recovered = true,
                    cause = %fuyao_hooks::panic_payload_to_string(&*payload),
                    "插件实例 register panic 已恢复"
                );
            }
        }

        // 3. 构造 SessionSender（identity 用 session_id 占位）
        //    注：各插件发 Plugin 消息的精确身份由插件通过 send_plugin_full 等方法控制，
        //    或后续给 SessionSender 加 with_identity 方法优化
        let sender = SessionSender::new(
            PluginEventSource {
                name: format!("session:{session_id}"),
            },
            tx_inbound,
            tx_interrupt,
            tx_plugin,
        );

        // 4. 把 sender 传给所有 send_input hook
        registry.init_send_inputs(sender).await;

        // 5. 包成 SharedHooks
        Arc::new(Mutex::new(registry))
    }
}
