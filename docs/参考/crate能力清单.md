# crate 能力清单

> 11 个 crate 的职责、依赖与公开 API 概要。详细 API 签名见 `cargo doc --workspace`。
>
> **可见性策略**：各 crate 内部模块均私有（`mod`），仅通过根层 `pub use` 导出公开符号。SDK 用户只依赖根层路径（如 `fuyao_api::AgentContext`），不可深入内部模块。

## 依赖层次

```text
L4  fuyao-app ──── 装配入口（依赖几乎所有下层）
       │
L3  fuyao-core（引擎内核）  fuyao-session  fuyao-tools
       │                       │              │
       │  core 不依赖 session/tools            │
       │                       └──────────────┘
       │                       tools 依赖 session
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
- **公开 API**：`AgentContext` / `AgentConfig` / `AgentPaths` / `ModelConfig` / `SharedAgentCtx`；`FuyaoConfig` 及全子配置；`get_config` / `set_config` / `load_config` / `load_env` / `load_merged_config`；`EventBase` / `InputEvent` / `OutputEvent` 及消息族；`Provider` trait / `Model`；`Session` / `Message` / `TodoItem`；`ToolDefinition` / `ToolResult` / `ToolCallContext`；`SkillDefinition` / `SkillMeta`；`AgentDefinition` / `AgentMode`；`MCPServerConfig`；`ApiError`

## fuyao-provider（L1 能力）

- **职责**：LLM 抽象 + 供应商注册 + 流式调用（reqwest 自建）
- **内部依赖**：api
- **公开 API**：`Provider` trait；`OpenAIProvider`；`ChatRequest` / `ChatResponse` / `StreamEvent`；`create_provider` / `create_provider_with_model` / `parse_model_id`；注册表 `register_provider` / `register_model` / `list_models` / `list_providers` / `get_provider` / `get_model`；`StreamDecoder`；重试 `backoff_duration` / `is_retryable`

## fuyao-mcp（L1 能力）

- **职责**：MCP 集成：MCPManager 管理多 Server
- **内部依赖**：api（+ rmcp 官方 SDK）
- **公开 API**：`MCPManager`（`new` / `from_config` / `start_all` / `stop_all` / `call_tool` / `refresh_tools` / `get_tool_definitions` / `get_server_status`）；`RegisteredTool`；`MCPManagerError`

## fuyao-skills（L1 能力）

- **职责**：Skills 三层发现 + frontmatter 解析
- **内部依赖**：api
- **公开 API**：`find_all_skills` / `find_skill_md_by_name`；`load_skill` / `load_skill_file`；re-export `SkillDefinition` / `SkillMeta`

## fuyao-hooks（L1 能力）

- **职责**：钩子（拦截 + 观察 + 主动）+ Plugin 系统
- **内部依赖**：api
- **公开 API**：`Plugin` trait；`PluginHost`（`add` / `install` / `dispose_all` / `list`）；`PluginEmitter`；`HooksRegistry`；`InterceptResult` / `BeforeLlmOutput` / `LlmErrorAction`；钩子签名 `BeforeLlmFn` / `OutputInterceptFn` / `OutputObserveFn` / `OnLlmErrorFn` / `SendInputFn`；`SharedHooks`

## fuyao-prompt（L2 构建）

- **职责**：提示词分层构建 + Agent 定义注册表
- **内部依赖**：api, skills
- **公开 API**：`build_system_prompt`；`load_agent_definition` / `load_agent_definition_from_agent_paths`；`AgentRegistry` + `AgentInfo` / `PagedAgents` / `UpdateContentRequest`；`PromptError`

## fuyao-guard（L2 构建）

- **职责**：行为防护：循环检测
- **内部依赖**：api, hooks
- **公开 API**：`LoopGuardPlugin`（impl `Plugin`）；re-export `LoopGuardConfig`

## fuyao-core（L3 内核）

- **职责**：引擎内核：dispatch / TurnExecutor(ReAct) / llm / tool_runner / interrupt
- **内部依赖**：api, provider, hooks
- **公开 API**：`Engine`；`EngineHandle`；re-export `SharedHooks` / `SharedTools` / `SharedAgentCtx`

## fuyao-session（L3 内核）

- **职责**：SQLite 持久化 + 缓存 + Todo + 上下文压缩
- **内部依赖**：api, hooks, prompt, provider
- **公开 API**：`SessionContext` / `SessionPlugin`；`SessionManager` / `get_session_manager`；`SQLiteStore`；`TodoStore`；`calculate_cost`

## fuyao-tools（L3 内核）

- **职责**：内置工具集 + 安全防护
- **内部依赖**：api, prompt, skills, session
- **公开 API**：`all_tools` / `all_tool_names` / `get_tool`；`ToolEntry`
- **内置工具**：read / write / edit / bash / grep / glob / webfetch / skill / todowrite

## fuyao-app（L4 装配）

- **职责**：装配入口：`start` / `init_engine` / `setup`
- **内部依赖**：api, core, guard, hooks, mcp, provider, session, tools
- **公开 API**：`start` / `init_engine` / `setup`；`AppContext`（`mcp_manager` / `plugin_host` / `log_guard`）；`LogGuard`；`InitError` / `SetupError`
