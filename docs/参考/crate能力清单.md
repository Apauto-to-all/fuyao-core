# crate 能力清单

> 11 个 crate 的职责、依赖与公开 API 概要。详细 API 签名见 `cargo doc --workspace`。
>
> **可见性策略**：各 crate 内部模块均私有（`mod`），仅通过根层 `pub use` 导出公开符号。SDK 用户只依赖根层路径（如 `fuyao_api::EngineParams`），不可深入内部模块。

## 依赖层次

```text
L4  fuyao-app ──── 装配入口（依赖几乎所有下层）
       │
L3  fuyao-core（引擎内核）  fuyao-session  fuyao-tools
       │                       │              │
       │  core 依赖 session（持 SessionStore 共享 DB）   │
       │                       └──────────────┘
       │                       tools 依赖 session（todo_store 共用 sessions.db）
       │
L2  fuyao-prompt    fuyao-guard
       │                │
L1  fuyao-provider  fuyao-mcp  fuyao-skills  fuyao-hooks
       │
L0  fuyao-api（零内部依赖）
```

> 依赖严格单向（上层 → 下层）。L3 三个 crate 平级，由 L4 装配组合。详见 [核心架构](../解释/核心架构.md)。

## fuyao-api（L0 基础）

- **职责**：公共类型 + 配置系统 + 路径系统
- **内部依赖**：无
- **公开 API**：
  - **Params 两件套**：`EngineParams` / `SessionParams`（内层 `ModelConfig` / `AgentConfig`）；`AgentPaths`
  - **配置**：`FuyaoConfig` 及全子配置（`CompressionConfig` / `SessionStorageConfig` / `TitleConfig` / `RetryConfig` / `LlmConfig` / `ToolsConfig` 等）；`get_config` / `set_config` / `load_config` / `load_env` / `load_merged_config`
  - **事件**：`EventBase` / `InputEvent`（User / Interrupt / Compress 三变体）/ `OutputEvent`（11 变体）及消息族（`InterruptSource`（User / Hook / Shutdown / Stop）/ `PluginSource` / `SystemSource` / `UserMessageMode` / `UserMessageSource` / `ChildSessionOrigin` / `ChildSessionState` / `CompressRequest` 等）
  - **控制通道载荷**：`ControlCommand`（单变体 `Compress`——手动压缩）/ `TurnDirective`（`Continue` / `StopTurn`——命令固有的 turn 处置指令）；仅在引擎内部控制通道流转，非消息（不属于 InputEvent / OutputEvent）
  - **Provider 类型**：`Provider` trait / `Model` / `ModelCost` / `ModelLimit` / `ThinkingType` 等
  - **会话类型**：`Session` / `Message`（含多模态图片附件 `images`）/ `MessageKind` / `ImageContent`（`{mime_type, data}`，data 为裸 base64，`from_data_url` 做入站归一）/ `TodoItem`
  - **子代理能力**：`SubagentOps` trait（`create_child_session` / `send` / `end_session`，工具 handler 经 `ToolCallContext` 持弱引用调用）/ `ChildSessionSource`（`Fresh` / `Fork(String)`）
  - **工具类型**：`ToolDefinition`（中立三要素 `name` / `description` / `parameters` 平铺，配 `ToolDefinitionBuilder` 链式构造）/ `ToolParameters` / `ToolParameterProperty` / `ToolCallData`（一次工具调用的中立表示：`id` / `name` / `arguments`，arguments 为 JSON 字符串）/ `ToolFn`（返回统一结果信封）/ `ToolOutput`（结果信封：`Value` JSON 对象 / `Text` 纯文本 / `Err` 错误）/ `ToolError`（错误主信息 + 附加字段，wire 恒有 `"error"` 键）/ `ToolCallContext`
  - **工具条目**：`ToolEntry`（`new` 组装 schema + handler + 可见性 / `name` 取 schema 名——工具名单一来源）+ `insert_tool`（以 schema 名为 key 注册进 map，重名 panic）
  - **handler 侧辅助**：`tool_handler`（规范签名异步函数一步包装成 `ToolFn`，吸收 `Arc` / `Box::pin` 闭包体操）/ `parse_args`（JSON 参数类型化解析为结构体，常见 serde 错误中文化）
  - **其他**：`AgentDefinition` / `AgentMode` / `SkillDefinition` / `SkillMeta` / `MCPServerConfig` / `ApiError` / `ConfigError`
  - **选择支持类型**：`AgentIdOption`（`{ id, source }`，source 为 `AgentIdSource`：`Global` / `Workspace`）/ `DefinitionOption`（`{ id, definition }`，无来源字段——定义按文件名做优先级覆盖，同名互斥、高优先级层胜出）/ `ModelOption`（`{ id, provider, model }`）——供 fuyao-app 的 `Discovery` / `list_agent_ids` 消费；`id` 为纯身份，agent_id 的层前缀由 `source` 独立承载

## fuyao-provider（L1 能力）

- **职责**：LLM 抽象 + 供应商注册表 + 多 Provider 路由 + 流式调用（reqwest 自建）+ 多模态图片请求适配（带图消息拼 `image_url` parts，MIME 白名单 / 单图 20MB 上限校验）
- **内部依赖**：api
- **公开 API**：
  - **trait**：`Provider`（`stream_chat` / `chat`）
  - **OpenAI 兼容实现**：`OpenAIProvider`
  - **请求响应类型**：`ChatRequest` / `ChatResponse` / `ChatMessage`（`tool_calls` 持 fuyao-api 的 `Vec<ToolCallData>`）/ `StreamEvent` / `StreamOptions`（`tools` 为 typed `Vec<ToolDefinition>`）/ `StreamUsage` / `FinishReason` / `BoxStream`
  - **错误**：`StreamError`（含 `Cancelled` 变体，shutdown 触发的非错误取消）/ `ProviderError` / `ClientError`
  - **工厂**：`create_provider` / `parse_model_id`
  - **注册表**：`register_provider` / `register_model` / `get_provider` / `get_model` / `list_providers` / `list_models` / `clear_cache` / `agent_paths_cache_key`
  - **多 Provider 路由**：`ProviderRegistry`（`from_registered` / `get` / `is_empty` / `provider_ids` / `with_instance`）—— 按 provider_id 索引的实例集合
  - **流式解码**：`StreamDecoder`
  - **重试辅助**：`backoff_duration` / `is_retryable`
  - **配置解析**：`get_base_url` / `resolve_api_key`

## fuyao-mcp（L1 能力）

- **职责**：MCP 集成：MCPManager 管理多 Server
- **内部依赖**：api（+ rmcp 官方 SDK）
- **公开 API**：`MCPManager`（`new` / `from_config` / `start_all` / `stop_all` / `call_tool`（返回 `Result<ToolOutput, MCPManagerError>`）/ `refresh_tools` / `get_tool_definitions` / `get_server_status` / `get_tool_entries`（直返 `Vec<ToolEntry>`，装配层零转换注入注册表）/ `disconnect`）；`RegisteredTool`；`MCPManagerError`

## fuyao-skills（L1 能力）

- **职责**：Skills 三层发现 + frontmatter 解析
- **内部依赖**：api
- **公开 API**：`find_all_skills` / `find_skill_md_by_name`；`load_skill` / `load_skill_file`；`LINKED_SUBDIRS`；re-export `SkillDefinition` / `SkillMeta`

## fuyao-hooks（L1 能力）

- **职责**：钩子（拦截 + 观察）+ Plugin 两层模型（工厂 + session 实例）+ 无状态插件快捷构造
- **内部依赖**：api
- **公开 API**：
  - **Plugin 两层模型**：`Plugin` trait（工厂模板，`name` / `create_instance`（每 session 生成独立实例）/ `dispose`（默认空，引擎 shutdown 最后逆序调用））；`PluginInstance` trait（session 级，`register(&mut HooksRegistry, &SessionSender)` 注册 hook 并接收绑定插件名的发送器 / `dispose`（默认空，session 结束时逆序调用））；`PluginHost`（引擎级工厂集合，`add` / `create_instances` 返回 `Vec<(插件名, 实例)>` 配对（生成前内置重名校验）/ `list` / `dispose_all`）；`PluginInstallError`
  - **无状态快捷路径**：`simple_plugin(name, register)` → `SimplePlugin`——一个注册闭包即插件（每 session 装配时调用一次注册各自的钩子）；无 per-session 状态的插件免写两层样板，与完整两层模型共存
  - **发消息能力**：`SessionSender`（绑定插件名 + 该 session 的入站 / 中断两条通道，方法 `send_user` / `send_user_with_mode` / `send_interrupt`，全部 try_send 非阻塞、source 自动按插件名追溯）
  - **钩子类型**：`HooksRegistry`（`new` / `register_output_intercept(priority, handler)` / `register_output_observe(priority, handler)`——两类钩子统一 priority 降序、同优先级按注册序 / `finalize`（装配期排序冻结））；钩子签名 `OutputInterceptFn`（`&mut OutputEvent` 原地修改，返回 `Option<String>`：`None` = 通过 / `Some(原因)` = 阻止）/ `OutputObserveFn`（`Arc<OutputEvent>` 共享只读，异步）
  - **`SharedHooks`**：`Arc<HooksRegistry>`（注册只发生在 session 装配期，finalize 后只读共享，运行期无锁）
  - **辅助**：`panic_payload_to_string`

## fuyao-prompt（L2 构建）

- **职责**：提示词分层构建 + Agent 定义加载 + Agent 定义注册表
- **内部依赖**：api, skills
- **公开 API**：`build_system_prompt`；`load_agent_definition` / `load_agent_definition_from_agent_paths`；`AgentRegistry` + `AgentInfo` / `PagedAgents` / `UpdateContentRequest`；`PromptError`

## fuyao-guard（L2 构建）

- **职责**：行为防护：循环检测
- **内部依赖**：api, hooks
- **公开 API**：`LoopGuardPlugin`（impl `Plugin` 工厂，每 session 生成独立 `LoopGuardInstance`：register 时注册 output_observe + output_intercept 两个钩子并保存 sender——interrupt 中断 / user 注入引导经 sender 发出，警告注入经 intercept 落进工具结果，通知走 tracing 日志）；re-export `LoopGuardConfig`

## fuyao-core（L3 内核）

- **职责**：引擎内核（两层分离）：能力共享层（Engine）+ 对话执行层（session task）
- **内部依赖**：api, provider, hooks, prompt, session
- **公开 API**：
  - **`Engine`**（创建 / 恢复 / 派生 / 子任务 / send / stop / end / shutdown，**无 recv**——出站走 per-session rx）：`new(params, providers, tools, plugin_host)` / `create_session(SessionParams)` → `(SessionId, rx)` / `resume_session(id, SessionParams)` → `(SessionId, rx)` / `fork_session(source_id, SessionParams)` → `(SessionId, rx)`（派生独立 session，`parent_session_id = None`）/ `create_child_session(parent_id, ChildSessionSource, SessionParams)` → `(SessionId, rx)`（创建子任务 session，`parent_session_id = Some(父 id)`；rx 不进 fan_out）/ `send(id, InputEvent)` / `stop_session(id, reason)`（屏障停 turn：返回即该 session 在跑 turn 已完全终止、中断收尾的补发落库（部分 AssistantMessage / 中断式 ToolResult）已全部完成——DB 静默。实现基础：session task 是该 session DB 写入的唯一执行者，turn 相位（TurnPhase，watch 通道）回 Idle 即静默。幂等语义是「确保静默」——session 未挂载或无 turn 在跑都直接 Ok。经中断通道投递 `InterruptSource::Stop` 信号等待相位回 Idle，10 秒未静默返 `StopTimeout`。不销毁 session（不写 ended_at、task 不退出），与 `end_session` 正交）/ `end_session(id, reason)` / `shutdown()`
  - **`ChildSessionSource`**：`Fresh`（空上下文）/ `Fork(SessionId)`（复制源可见消息 + system_prompt）
  - **`SessionId`**：`String` 别名
  - **`EngineError`**：`SessionNotFound` / `Storage` / `Provider` / `Shutdown` / `StopTimeout { session_id, timeout_secs }`（stop_session 等待 turn 静默超时——turn 卡在不响应中断信号的环节；调用方不得继续依赖静默前提做后续 DB 操作，可重试停止）
  - **历史回放投影**：`messages_to_events(Vec<Message>) -> Vec<OutputEvent>`——存储 Message → 与实时流同构的事件流（seq 正序），上层会话历史查询接口消费
  - **工具注册**：`ToolRegistry` / `ToolRegistryBuilder`（条目类型 `ToolEntry` 由 fuyao-api 提供，见上文工具条目）
  - **插件相关重导出**：`Plugin` / `PluginHost` / `PluginInstance` / `SessionSender` / `SharedHooks`

> 引擎内核内部的 dispatch 管道（拦截→发送→观察）、history 模块（事件↔Message 双向映射 + 计费 + 进历史统一入口）、ReAct 循环（双队列 + 中断 + 重试）、tool_exec（智能调度）等模块为 crate 私有，仅通过上述根层 API 暴露。

## fuyao-session（L3 内核）

- **职责**：SQLite 持久化 + 上下文压缩 + 费用统计 + 标题生成
- **内部依赖**：api
- **公开 API**：
  - **存储层**：`SessionStore`（`new(db_path)` / `pool()` 共享连接池 / `create` / `get` / `update`（落库时经 `unixepoch()` 刷新 `last_active_at`）/ `delete` / `list_all(workspace_filter, limit, offset)`（按 `last_active_at` 倒序 + 可选按 workspace 过滤）/ `count_with_filter(workspace_filter)` / `insert_message` / `count_user_messages`（user 角色计数，标题首轮判定用）/ `load_full_history`（全量审计，seq 升序）/ `list_messages_before(session_id, before_seq, limit)`（游标分页浏览，seq 倒序）/ `load_visible_messages`（LLM 可见窗口，压缩感知单边界查询）/ `mark_compaction` / `rollback_to(session_id, target_seq)`（对话回退，删目标消息及其后消息 + 重算 count 类与压缩元数据，无返回载荷——后续状态经读路径获取；token / cost 消费类字段原值保留；消费方是会话管理门面 `SessionManager::rollback_session`）/ `update_system_prompt` / `update_title` / `end_session`）
  - **压缩模块**：`should_compress` / `generate_summary` / `apply`
  - **费用统计**：`calculate_cost`（单条消息费用，Decimal 精确）/ `fill_message_cost`（按 msg 已填 token 字段算 cost 填入——token 字段由 history 映射自事件 payload 先行填好）
  - **标题生成**：`maybe_generate_title`
  - **错误**：`SessionError`（`IoError` / `SqlxError` / `NotFound` / `InvalidRollbackTarget`）

## fuyao-tools（L3 内核）

- **职责**：内置工具集 + 安全防护 + todo 持久化（自建 TodoStore）
- **内部依赖**：api, prompt, skills, sqlx
- **公开 API**：`all_tools` / `all_tool_names` / `get_tool`（静态注册表 `HashMap<String, ToolEntry>`，key 取 schema 名）
- **内置工具**：read / write / edit / bash / grep / glob / webfetch / skill / todowrite / **subagent**（子代理工具，派生子 session 执行独立子任务，`child_invisible = true` 递归防护）

## fuyao-app（L4 装配）

- **职责**：装配入口 + fan-in 单一出口：`start` / `init_engine` / `build_tool_registry` / `App` / `SessionManager` / `Discovery` / `list_agent_ids`
- **内部依赖**：api, core, guard, hooks, mcp, provider, prompt, session, tools
- **公开 API**：
  - **`start(EngineParams)`**：一行启动（`init_engine` → `build_tool_registry` → 装配 `LoopGuardPlugin` → 创建 `SessionStore` → `Engine::new`（注入 store）→ `App::new` → `SessionManager::new` → `Discovery::new`），返回 `FuyaoApp { app, sessions, discovery }`——上层同时拿到运行时入口（`app`）、查询入口（`sessions`）、选择支持入口（`discovery`）；`app` 与 `sessions` 共享同一份 `Arc<SessionStore>`
  - **`FuyaoApp`**（`start` 的聚合产物）：`app: App`（运行时交互：create/send/stop/recv/end）+ `sessions: SessionManager`（会话检索：list/count）+ `discovery: Discovery`（选择支持：列 Agent 定义 / model），平级正交、互不依赖
  - **`App`**（运行时交互门面，持 `Engine` + fan-in 出口）：`new(engine, mcp_manager, log_guard)` / `create_session(SessionParams)` → `SessionId`（rx 由内部 forwarder 消费进 fan_out）/ `resume_session` / `fork_session` / `create_child_session(parent, source, params)` → `(SessionId, rx)`（**rx 不进 fan_out**，返调用方独占消费）/ `send` / `stop_session(id, reason)`（屏障停 turn，直接代理 `Engine::stop_session`；返回即该 session 的 DB 已静默，session 保持存活。管理型同步方法，不走消息总线；「先停后改库」两步编排（如回退：先 stop 再 `SessionManager::rollback_session`）的第一步）/ `recv()` → `Option<OutputEvent>`（单一出口）/ `end_session` / `shutdown(self)`（两段式：engine.shutdown → forwarder 退出 → 停 MCP → drop log_guard）
  - **`SessionManager`**（会话管理门面，持同一份 `Arc<SessionStore>`，与 `App` 平级正交）：`new(store)` / `get_session(session_id)` → `Option<Session>`（单行元数据直读）/ `list_sessions(workspace_filter, limit, offset)` → `Vec<Session>`（按 `last_active_at` 倒序，可选按 workspace 过滤）/ `session_count(workspace_filter)` → `i64` / `update_title(session_id, new_title)`（应用 / 用户手动改名）/ `delete_session(session_id)`（单事务 cascade 删 todos + messages + session 行）/ `rollback_session(session_id, target_seq)` → `()`（对话回退：复用 `SessionStore::rollback_to` 单事务原子执行体，无返回载荷——回退后的会话状态经既有读路径获取（`list_messages` 看剩余消息流、`get_session` 看重算后的 session 行），目标用户消息本体调用方在选定回退点时即持有。目标约束：只能回退到 user 或 compaction 消息，校验以 `kind` 为准。**不校验该 session 是否有活跃 turn**——turn 运行中回退会与后续落库竞争；安全回退的调用顺序是「先停后滚」：先 `App::stop_session` 屏障停 turn，再做本调用，两步之间无该 session 的并发写库。错误：`SessionError::NotFound`（session 或 target_seq 无对应消息）/ `SessionError::InvalidRollbackTarget`（目标非 user 且非 compaction））/ `list_messages(session_id, before_seq: Option<i64>, limit: Option<i64>)` → `Vec<Message>`（游标分页，seq 倒序；`before_seq=None` 取最新一页，`Some(N)` 向前翻；`limit=None` 用默认 50；compaction 消息正常显示不过滤；不提供总数，下一页用返回条数 == limit 判断）/ `list_events(session_id, before_seq, limit)` → `EventPage`（与 list_messages 同源取数同游标，经 `fuyao_core::messages_to_events` 把 Message 投影成与实时流同构的 `OutputEvent`，前端历史回放与实时流共用一套渲染；`has_more` / `next_cursor` 复用 list_messages 推导）
  - **`init_engine(EngineParams)`**：配置 / 日志 / Provider 准备，返回 `InitResult { provider: ProviderRegistry, log_guard }`；入口先做 agent_id 校验（来源前缀必须显式：`global/{名}` / `workspace/{名}`，大小写不敏感；workspace 来源须配 workspace 参数），非法即 fail-fast 报错
  - **`build_tool_registry()`**：收集内置 + MCP 工具，返回 `(ToolRegistry, Option<Arc<MCPManager>>)`
  - **`list_agent_ids(&AgentPaths)`**：列举可选 agent_id（启动前可用，不依赖引擎）。接收应用层构造的 `AgentPaths`，按其 workspace / fuyao_home 扫描 `fuyao-agents/`，返回 `Vec<AgentIdOption>`（纯名 id + 来源层 `source`，项目层同名覆盖全局层，按 id 升序）。应用层用同一份 `AgentPaths` 先列 id、再造 `EngineParams` 启动，保证列举基准与启动基准一致
  - **`Discovery`**（选择支持门面，`FuyaoApp.discovery` 字段，持 `start` 注入的 `AgentPaths`）：`list_primary_definitions()` → `Vec<DefinitionOption>`（会话人格专用，仅 Primary 模式，四层定义目录 + 内置，零参数）/ `list_subagent_definitions()` → `Vec<DefinitionOption>`（子代理工具候选全集，仅 Subagent 模式，与主代理列举按 mode 互斥）/ `list_models()` → `Vec<ModelOption>`（Provider 注册缓存，启动前为空，零参数）；路径身份启动时注入一次，所有查询共用，调用方不再传参
  - **`LogGuard`**：drop 时 flush 文件日志
  - **错误**：`InitError`（`NoProviderAvailable` / `ConfigError` / `InvalidAgentId`）/ `SetupError`（`Init`）
