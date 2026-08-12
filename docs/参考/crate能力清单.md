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
  - **事件**：`EventBase` / `InputEvent`（User / Interrupt / Plugin / Compress 四变体）/ `OutputEvent`（12 变体）及消息族（`InboundUser` / `InterruptSource` / `PluginEventSource` / `UserMessageMode` / `UserMessageSource` / `ChildSessionOrigin` / `ChildSessionState` / `CompressRequest` 等）
  - **Provider 类型**：`Provider` trait / `Model` / `ModelCost` / `ModelLimit` / `ThinkingType` 等
  - **会话类型**：`Session` / `Message`（含多模态图片附件 `images`）/ `MessageKind` / `ImageContent`（`{mime_type, data}`，data 为裸 base64，`from_data_url` 做入站归一）/ `TodoItem`
  - **子代理能力**：`SubagentOps` trait（`create_child_session` / `send` / `end_session`，工具 handler 经 `ToolCallContext` 持弱引用调用）/ `ChildSessionSource`（`Fresh` / `Fork(String)`）
  - **工具类型**：`ToolDefinition` / `ToolSchema` / `ToolParameters` / `ToolParameterProperty` / `ToolFn` / `ToolResult` / `ToolCallContext`
  - **其他**：`AgentDefinition` / `AgentMode` / `SkillDefinition` / `SkillMeta` / `MCPServerConfig` / `ApiError` / `ConfigError`
  - **选择支持类型**：`AgentIdOption`（`{ id, source }`）/ `DefinitionOption`（`{ id, source, definition }`）/ `ModelOption`（`{ id, provider, model }`）/ `Source`（来源层：`Workspace` / `Agent` / `Global` / `Extra` / `Builtin`）——供 fuyao-app 的 `Discovery` / `list_agent_ids` 消费；`id` 为纯身份，来源层独立承载

## fuyao-provider（L1 能力）

- **职责**：LLM 抽象 + 供应商注册表 + 多 Provider 路由 + 流式调用（reqwest 自建）+ 多模态图片请求适配（带图消息拼 `image_url` parts，MIME 白名单 / 单图 20MB 上限校验）
- **内部依赖**：api
- **公开 API**：
  - **trait**：`Provider`（`stream_chat` / `chat`）
  - **OpenAI 兼容实现**：`OpenAIProvider`
  - **请求响应类型**：`ChatRequest` / `ChatResponse` / `ChatMessage` / `StreamEvent` / `StreamOptions` / `StreamUsage` / `ToolCallData` / `FinishReason` / `BoxStream`
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
- **公开 API**：`MCPManager`（`new` / `from_config` / `start_all` / `stop_all` / `call_tool` / `refresh_tools` / `get_tool_definitions` / `get_server_status` / `get_tool_entries` / `disconnect`）；`RegisteredTool`；`MCPManagerError`

## fuyao-skills（L1 能力）

- **职责**：Skills 三层发现 + frontmatter 解析
- **内部依赖**：api
- **公开 API**：`find_all_skills` / `find_skill_md_by_name`；`load_skill` / `load_skill_file`；`LINKED_SUBDIRS`；re-export `SkillDefinition` / `SkillMeta`

## fuyao-hooks（L1 能力）

- **职责**：钩子（拦截 + 观察 + 主动）+ Plugin 两层模型（工厂 + session 实例）
- **内部依赖**：api
- **公开 API**：
  - **Plugin 两层模型**：`Plugin` trait（工厂模板，`create_instance` 生成 session 独立实例）；`PluginInstance` trait（session 级，`register` 注册 hook 到该 session 私有 HooksRegistry）；`PluginHost`（引擎级工厂集合，`add` / `create_instances` / `list`）；`PluginInstallError`
  - **发消息能力**：`SessionSender`（绑定该 session 的三条通道 tx_inbound/tx_interrupt/tx_plugin，方法 `send_user` / `send_user_with_mode` / `send_interrupt` / `send_plugin` / `send_plugin_data` / `send_plugin_full`）
  - **钩子类型**：`HooksRegistry`（`new` / `init_send_inputs`）；`InterceptResult`（`Pass` / `Block`）；钩子签名 `OutputInterceptFn` / `OutputObserveFn` / `SendInputFn`
  - **`SharedHooks`**：`Arc<tokio::sync::Mutex<HooksRegistry>>`
  - **辅助**：`panic_payload_to_string`

## fuyao-prompt（L2 构建）

- **职责**：提示词分层构建 + Agent 定义加载 + Agent 定义注册表
- **内部依赖**：api, skills
- **公开 API**：`build_system_prompt`；`load_agent_definition` / `load_agent_definition_from_agent_paths`；`AgentRegistry` + `AgentInfo` / `PagedAgents` / `UpdateContentRequest`；`PromptError`

## fuyao-guard（L2 构建）

- **职责**：行为防护：循环检测
- **内部依赖**：api, hooks
- **公开 API**：`LoopGuardPlugin`（impl `Plugin` 工厂，每 session 生成独立 `LoopGuardInstance` 持 per-session 计数器）；re-export `LoopGuardConfig`

## fuyao-core（L3 内核）

- **职责**：引擎内核（两层分离）：能力共享层（Engine）+ 对话执行层（session task）
- **内部依赖**：api, provider, hooks, prompt, session
- **公开 API**：
  - **`Engine`**（创建 / 恢复 / 派生 / 子任务 / send / end / shutdown，**无 recv**——出站走 per-session rx）：`new(params, providers, tools, plugin_host)` / `create_session(SessionParams)` → `(SessionId, rx)` / `resume_session(id, SessionParams)` → `(SessionId, rx)` / `fork_session(source_id, SessionParams)` → `(SessionId, rx)`（派生独立 session，`parent_session_id = None`）/ `create_child_session(parent_id, ChildSessionSource, SessionParams)` → `(SessionId, rx)`（创建子任务 session，`parent_session_id = Some(父 id)`；rx 不进 fan_out）/ `send(id, InputEvent)` / `end_session(id, reason)` / `shutdown()`
  - **`ChildSessionSource`**：`Fresh`（空上下文）/ `Fork(SessionId)`（复制源可见消息 + system_prompt）
  - **`SessionId`**：`String` 别名
  - **`EngineError`**：`SessionNotFound` / `Storage` / `Provider` / `Shutdown`
  - **工具注册**：`ToolRegistry` / `ToolRegistryBuilder` / `ToolEntry`
  - **插件相关重导出**：`Plugin` / `PluginHost` / `PluginInstance` / `SessionSender` / `SharedHooks`

> 引擎内核内部的 dispatch 管道（拦截→处理→发送→观察）、ReAct 循环（双队列 + 中断 + 重试）、tool_exec（智能调度）等模块为 crate 私有，仅通过上述根层 API 暴露。

## fuyao-session（L3 内核）

- **职责**：SQLite 持久化 + 上下文压缩 + 费用统计 + 标题生成
- **内部依赖**：api
- **公开 API**：
  - **存储层**：`SessionStore`（`new(db_path)` / `pool()` 共享连接池 / `create` / `get` / `update`（落库时经 `unixepoch()` 刷新 `last_active_at`）/ `delete` / `list_all(workspace_filter, limit, offset)`（按 `last_active_at` 倒序 + 可选按 workspace 过滤）/ `count` / `count_with_filter(workspace_filter)` / `insert_message` / `count_messages` / `load_full_history`（全量审计，seq 升序）/ `list_messages_before(session_id, before_seq, limit)`（游标分页浏览，seq 倒序）/ `load_visible_messages`（LLM 可见窗口，压缩感知动态拼接）/ `mark_compaction` / `rollback_to(session_id, target_seq)`（对话回退，删目标 seq 之后消息 + 重算 count 类与压缩元数据，返回 `RollbackPayload`）/ `update_system_prompt` / `update_title` / `end_session`）
  - **压缩模块**：`should_compress` / `generate_summary` / `apply`
  - **费用统计**：`calculate_cost` / `fill_message_cost` / `accumulate_session_total`
  - **标题生成**：`maybe_generate_title`
  - **错误**：`SessionError`（`IoError` / `SqlxError` / `InvalidState` / `NotFound`）

## fuyao-tools（L3 内核）

- **职责**：内置工具集 + 安全防护 + todo 持久化（自建 TodoStore）
- **内部依赖**：api, prompt, skills, sqlx
- **公开 API**：`all_tools` / `all_tool_names` / `get_tool`；`ToolEntry`
- **内置工具**：read / write / edit / bash / grep / glob / webfetch / skill / todowrite / **subagent**（子代理工具，派生子 session 执行独立子任务，`child_invisible = true` 递归防护）

## fuyao-app（L4 装配）

- **职责**：装配入口 + fan-in 单一出口：`start` / `init_engine` / `build_tool_registry` / `App` / `SessionManager` / `Discovery` / `list_agent_ids`
- **内部依赖**：api, core, guard, hooks, mcp, provider, prompt, session, tools
- **公开 API**：
  - **`start(EngineParams)`**：一行启动（`init_engine` → `build_tool_registry` → 装配 `LoopGuardPlugin` → 创建 `SessionStore` → `Engine::new`（注入 store）→ `App::new` → `SessionManager::new` → `Discovery::new`），返回 `FuyaoApp { app, sessions, discovery }`——上层同时拿到运行时入口（`app`）、查询入口（`sessions`）、选择支持入口（`discovery`）；`app` 与 `sessions` 共享同一份 `Arc<SessionStore>`
  - **`FuyaoApp`**（`start` 的聚合产物）：`app: App`（运行时交互：create/send/recv/end）+ `sessions: SessionManager`（会话检索：list/count）+ `discovery: Discovery`（选择支持：列 Agent 定义 / model），平级正交、互不依赖
  - **`App`**（运行时交互门面，持 `Engine` + fan-in 出口）：`new(engine, mcp_manager, log_guard)` / `create_session(SessionParams)` → `SessionId`（rx 由内部 forwarder 消费进 fan_out）/ `resume_session` / `fork_session` / `create_child_session(parent, source, params)` → `(SessionId, rx)`（**rx 不进 fan_out**，返调用方独占消费）/ `send` / `recv()` → `Option<OutputEvent>`（单一出口）/ `end_session` / `shutdown(self)`（两段式：engine.shutdown → forwarder 退出 → 停 MCP → drop log_guard）
  - **`SessionManager`**（会话检索门面，持同一份 `Arc<SessionStore>`，与 `App` 平级正交）：`new(store)` / `list_sessions(workspace_filter, limit, offset)` → `Vec<Session>`（按 `last_active_at` 倒序，可选按 workspace 过滤）/ `session_count(workspace_filter)` → `i64` / `list_messages(session_id, before_seq: Option<i64>, limit: Option<i64>)` → `Vec<Message>`（游标分页，seq 倒序；`before_seq=None` 取最新一页，`Some(N)` 向前翻；`limit=None` 用默认 50；compaction 消息正常显示不过滤；不提供总数，下一页用返回条数 == limit 判断）
  - **`init_engine(EngineParams)`**：配置 / 日志 / Provider 准备，返回 `InitResult { provider: ProviderRegistry, log_guard }`
  - **`build_tool_registry()`**：收集内置 + MCP 工具，返回 `(ToolRegistry, Option<Arc<MCPManager>>)`
  - **`list_agent_ids(&AgentPaths)`**：列举可选 agent_id（启动前可用，不依赖引擎）。接收应用层构造的 `AgentPaths`，按其 workspace / fuyao_home 扫描 `fuyao-agents/`，返回 `Vec<AgentIdOption>`（纯名 id + 来源层 `source`，项目层同名覆盖全局层，按 id 升序）。应用层用同一份 `AgentPaths` 先列 id、再造 `EngineParams` 启动，保证列举基准与启动基准一致
  - **`Discovery`**（选择支持门面，`FuyaoApp.discovery` 字段，持 `start` 注入的 `AgentPaths`）：`list_definitions()` → `Vec<DefinitionOption>`（四层定义目录 + 内置，零参数）/ `list_models()` → `Vec<ModelOption>`（Provider 注册缓存，启动前为空，零参数）；路径身份启动时注入一次，两个查询共用，调用方不再传参
  - **`LogGuard`**：drop 时 flush 文件日志
  - **错误**：`InitError`（`NoProviderAvailable` / `ConfigError`）/ `SetupError`（`Init`）
