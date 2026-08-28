# 领域上下文（CONTEXT）

fuyao-core 是**独立 Agent 引擎 SDK**——配置好模型就能跑的独立 Agent，核心只负责单个 Agent 的生命周期（ReAct 循环、会话、工具、LLM 调用），保持轻量。

> 本文件是项目的统一语言词汇表：issue 标题、重构提案、假设、测试名涉及领域概念时，使用下表术语原文，不要漂移到同义改写。若所需概念不在表中，这是一个信号——要么在发明项目不用的语言（请重新考虑），要么存在真实缺口（交给 `/domain-modeling` 补录）。

## 词汇表

### 引擎与生命周期

| 术语 | 代码标识 | 含义 |
| --- | --- | --- |
| 引擎 | `Engine` | 能力共享层，启动一次装配 provider / store / 工具 / 插件工厂；对外四动作 `create_session` / `resume_session` / `send` / `shutdown`，另有 `create_child_session` / `stop_session` / `reload_providers` 等扩展 |
| ReAct 循环 | `react` 模块 | 每 session 一个 tokio task：想 → 调一批工具 → 消费 guide 队列 → 再想 → 最终回复 |
| 轮次 | `run_turn` / `TurnOutcome` | 单轮 ReAct 的执行与退出原因（`Completed` / `Interrupted` / `Failed`） |
| dispatch 管道 | `dispatch` | 统一输出处理链：`intercept`（同步原地修改 / 阻止）→ `deliver`（发送 + 观察） |
| 输出事件 | `OutputEvent` | Engine → UI 的唯一对外事件，13 个变体（`Chunk` / `User` / `Control` / `ToolCall` / `ToolResult` / `Assistant` / `Interrupt` / `Error` / `PluginNotice` / `Compression` / `Title` / `Retry` / `ChildSession`） |
| 输入事件 | `InputEvent` | UI → Engine 的入口事件（`User` / `Interrupt` / `Control`），入口即转 `OutputEvent`，内核不区分方向 |
| 控制命令 | `ControlCommand` / `ControlMessage` | 命令主循环做事的消息（如手动压缩）：与用户消息同型排队、同序消费，消费点先以 `OutputEvent::Control` 回显对外、后执行命令本体，执行产物照常走输出事件流 |
| 控制命令附言 | `ControlPayload.note` | 发送方随命令附带的可选自由文本（如手动压缩的摘要侧重要求）：不落库、回显原样携带，是否消费由各命令自决——多数命令视作一段提示词交给 AI 自行理解；新增命令禁止默认把附言设为必填（见 ADR-0001） |
| 事件信封 | `EventBase` | 每条事件带 `seq`（落库回填序号）/ `timestamp` / `session_id`（全程标签） |
| 中断协议 | `interrupt` 模块 | 三段 select! 收尾：`finish_streaming` / `finish_tool_batch` / `notify_idle`，先发 Interrupt 通知再补增量结果落库 |
| 停止屏障 | `stop_session` | 返回即该 session DB 已静默（等 TurnPhase 回 Idle），是「先停后改库」组合操作的前半步 |

### 会话与消息

| 术语 | 代码标识 | 含义 |
| --- | --- | --- |
| 会话 | `Session` / `SessionHandle` | 一次对话实体；Handle 是引擎调度表条目（双队列 + 两通道 + 参数句柄） |
| session 通道 | inbound / interrupt | 统一入站通道（外部用户 + 控制命令与插件注入的 User 条目统一 `QueueEntry` 承载，保证总序；`QueueEntry` 定义在 fuyao-api）/ 中断通道（与队列正交） |
| 存储层 | `SessionStore` | SQLite（WAL）唯一入口；sessions / messages / todos 三表 |
| 落库序号 | `seq` | 事务内分配，事件级落库后回填到 `EventBase` |
| 可见窗口 | `load_visible_messages` | 给 LLM 的压缩感知窗口（最新 compaction 摘要 + 其后新消息），与「给人看的」查询路径正交 |
| 配对兜底 | `pair_missing_tool_results` | wire 消息序列中为缺结果的 tool_call 补占位 tool_result（content 固定「[工具执行被拦截或中断]」标记，读时合成不落库）；主对话与压缩两路共用同一函数——被拦截 / 中断 / 崩溃留下的悬挂对不破协议配对，两路请求前缀序列同口径 |
| 上下文压缩 | `compaction` / `run_compression` | 插一条 `kind='compaction'` 边界消息 + 更新元数据，旧消息物理保留；触发公式 `prompt_tokens >= threshold × (context_length - summary_max_tokens)` |
| 回退 | `rollback_to` | 删目标（user 或 compaction 边界）及其后消息；计数类重算，费用不抹账（回退不抹账） |
| 派生 | `fork_session` / `fork_to` | 复制源会话到新独立主会话（parent=None），非破坏（源不动）。两个面：SessionManager / 存储层按目标消息切割复制（`seq < target` 全部消息，纯存储操作非活装配，续聊需 `resume_session`，目标必须是 user / compaction 消息，与回退共用目标校验）；`create_child_session` 的 `Fork` 源复制为子会话（活装配可直接对话，带父标记） |
| 子会话 | `create_child_session` | 带父标记（`parent_session_id`），rx 不进 fan-in；`Fresh`（空上下文）/ `Fork`（复制）两源 |
| 双队列 | guide / pending | 用户消息与控制命令消息共用的排队层：条目（`QueueEntry`）自带 mode 决定入队与生效时机——`Guide`（引导队列，工具批完成后即投递）/ `Pending`（排队队列，最终回复后才投递） |
| 计费 | `calculate_cost` | 全部费用运算集中于此，Decimal 精确，按 `PriceTier` 分档；assistant 消息经 `emit_billed_to_history` 唯一计费时机 |
| 标题生成 | `maybe_spawn_title` | 首轮 user 消息落库后 fire-and-forget，每 session 一次（`title_gate`） |
| 历史回放 | `messages_to_events` | DB 消息反向投影成事件流，与实时流同构，对外唯一出口 |

### Agent 身份与定义（易混概念，重点）

| 术语 | 代码标识 | 含义 |
| --- | --- | --- |
| agent_id | `AgentPaths` | 独立 Agent 实体（**数据隔离单元**）：`global/{名}` / `workspace/{名}`（来源前缀必须显式），决定 sessions.db / 缓存 / 日志选址 |
| Agent 定义 | `AgentDefinition` | 提示词人格（**一项能力**），终端用户称「智能体」；`agents/{name}.md`（frontmatter + 正文） |
| 四层解析 | `LayeredPaths` | 同名定义查找链 workspace > agent > global > extra，最低优先级为内置表 |
| 内置定义 | `builtin::builtin_agent_md` | 编译期 `include_str!` 嵌入的 `default` / `explore` / `executor`，经统一内置资产模块承载；`default` 是框架保证恒存在的出厂主定义 |
| 定义模式 | `AgentMode` | `Primary`（主代理人格）/ `Subagent`（专职子代理），职责互斥 |
| 子代理 | `subagent` 工具 | 派生 child session 跑专职定义，最终回复以 `ToolOutput::text` 回喂父循环；`child_invisible=true` 递归防护 |

### 供应商与模型

| 术语 | 代码标识 | 含义 |
| --- | --- | --- |
| 供应商 | `Provider` trait | LLM 统一抽象：`stream_chat`（返 `StreamEvent` 流）+ `chat`（非流式） |
| API 协议 | `ApiProtocol` | 供应商的 wire 协议方言：`openai-completions` / `anthropic-messages` 两枚举，供应商段必填（无缺省，缺失即配置错误） |
| 三桶归一化 | `StreamUsage` | Anthropic 输入 token 是互斥三桶（`input_tokens` 仅含最后缓存断点之后的部分），adapter 层求和归一：总 prompt = `cache_read + cache_creation + input`；`prompt_cached_tokens` 取 cache_read，`prompt_cache_creation_tokens` 留痕不消费（见 ADR-0003） |
| 供应商注册表 | `ProviderRegistry` | 引擎级路由；`model_id` 形如 `provider_id/model_id`，按前缀拆解路由；运行时可 `register` / `unregister` 即时生效 |
| admin 域 | `admin` 模块 | 供应商管理底座：`ProviderSpec` 完整期望状态写回 global 层（`fuyao.toml` + `.env` 两落点） |
| 密钥隔离 | `env_file` | 明文密钥只进 `.env`，toml 只写 `api_key_env_var` 指针；`.env` 只增改不删除；供应商 id 不可变（换 id 走建新 + 删旧） |
| 流事件 | `StreamEvent` | `TextDelta` / `ReasoningDelta` / `ToolCallChunk` / `Done`；`StreamAggregator` 负责聚合成 `OutputEvent` |
| 重试 | `RetryRunner` | 可恢复错误（限流 / 超时 / 连接 / 5xx）退避重试，前发 `Retry` 事件；首 chunk 后错误立即冒泡 |
| 按用途模型 | `[models.fast]` | 轻量模型引用（标题生成等），主对话模型不在此配置，创建会话时显式给 |

### 工具与能力

| 术语 | 代码标识 | 含义 |
| --- | --- | --- |
| 工具条目 | `ToolEntry` | schema + handler + `child_invisible` 可见性 |
| 工具定义 | `ToolDefinition` | JSON Schema 中立三要素，wire 编码归供应商适配层 |
| 工具结果 | `ToolOutput` | `Value`（JSON）/ `Text`（纯文本）/ `Err`（wire 上恒有 `"error"` 键）；`to_wire()` 单点序列化 |
| 内置工具 | 10 个 | `read` / `write` / `glob` / `grep` / `edit` / `bash` / `skill` / `subagent` / `todowrite` / `webfetch` |
| MCP 工具 | `mcp_{server}_{tool}` | 前缀命名；MCPConnection 长连接自动重连，实例级熔断 `CircuitBreaker` |
| 技能 | Skills | Agent Skills 协议三层渐进披露：Tier 1 元数据发现 → Tier 2 完整内容 → Tier 3 关联文件按需加载 |
| 内置资产模块 | `fuyao_prompt::builtin` | 所有编译期内置资产的唯一上车道：资产目录与运行时目录约定同构（assets/agents/{name}.md、assets/skills/{name}/SKILL.md），单一静态表派生名字清单与查找（无第二份清单可漂移）；内置恒为解析链最低优先级兜底、四层文件系统同名覆盖、零落盘 |
| 工具上下文 | `ToolCallContext` | 编排层注入（session_id / agent_paths / 能力聚合：`subagent_ops` 弱引用 / `event_forwarder` / `todo_store`） |
| 并行调度 | `should_parallelize` | 智能判定（never_parallel / 路径重叠 / parallel_safe）→ JoinSet 并发或串行；「完成一个通知一个」 |

### 钩子与防护

| 术语 | 代码标识 | 含义 |
| --- | --- | --- |
| 拦截钩子 | `OutputInterceptFn` | 同步串行、`&mut` 原地修改、返回 `Some(reason)` 即阻止整条丢弃 |
| 观察钩子 | `OutputObserveFn` | 异步串行、`Arc` 共享只读（多钩子零拷贝），panic + 超时防护 |
| 插件两层 | `Plugin` / `PluginInstance` | 引擎级工厂模板 / session 级实例（持 per-session 独立状态）；注册仅发生在装配期，`finalize` 后冻结 |
| 循环防护 | `LoopGuardPlugin` | 工具重复检测（连续相同调用 / A→B→A→B 序列模式）+ 文本自相似检测（n-gram） |
| 处置升级 | `LoopSeverity` | 四级：`Warn`（追加提示）/ `Inject`（替换工具结果）/ `Interrupt`（发中断）/ `Abort`（彻底终止）；已中断 ≥3 次升级为 Abort |

### 配置分层

| 术语 | 代码标识 | 含义 |
| --- | --- | --- |
| 三层合并 | `load_merged_config` | `fuyao.toml`：global（`~/.fuyao/`）→ agent 目录 → workspace，递归深合并（嵌套字段级，数组整体覆盖） |
| 供应商单点 | `[providers]` | 只允许出现在 global 层（单一事实源），他层出现即加载报错；分层的是「选择」，单点的是「定义」 |
| 全局句柄 | `set_config` / `get_config` | `OnceLock<Arc<FuyaoConfig>>` 进程级只读配置 |
| 装配门面 | `FuyaoApp` | 一键装配（init → 收集工具 → 装插件 → 启动引擎），三门面：`app`（运行时）/ `sessions`（会话管理）/ `discovery`（选择支持）；供应商管理（`ProviderManager`）为独立构造的管理入口（纯文件读写，不依赖引擎） |
| fan-in 汇聚 | `App::recv` | 主 session 的 per-session rx 汇聚进单一 fan_out 通道（容量 512），上层 UI 单一出口消费 |

## 关键不变量

1. **DB 唯一数据源**——事件级落库，无常驻内存历史；一切统计 / 回放 / 可见窗口从 DB 查询
2. **前缀缓存红线**——system_prompt / 工具集 / agent 配置在 session 创建时定死；压缩摘要走固定策略（消息原样发 + 末尾追加摘要指令），保前缀缓存生命线
3. **fail-loud**——未知名 / 坏配置启动即报错（错误附可用列表），绝不静默替换或降级
4. **消息类型只认输出侧**——`InputEvent` 入口即转 `OutputEvent`，内核不引入输入侧类型
5. **引擎是忠实执行器**——忠实触发外部一切命令，不做去重 / 合并 / 冷却等意图解释，那属于上层职责
6. **新生命周期信号首选加事件变体**——而非给现有 payload 挂额外字段（消息驱动架构）
7. **依赖严格单向**——11 个 crate 分五类（基座 → 内核 → 协作者 → 能力 → 装配），禁止反向依赖
8. **所有消息都可以被拦截**——命令消费的对外回显与执行产物照常过 dispatch 管道（可被拦截钩子修改或阻止）；回显被丢弃只影响对外可见性，命令本体忠实执行不受影响；引擎不为命令开拦截豁免、也不新增拦截扩展，后续有必要再附加

## 一条消息的旅程

1. `App::send(session_id, InputEvent::User)` → `Engine::send` 转化为 output 侧消息，包 `QueueEntry` 投统一入站通道（`InputEvent::Control` 同路；插件注入的 User 消息经 `SessionSender` 也投同一通道——发送顺序即排队顺序）
2. session task 按条目自带 mode 入 guide / pending 双队列
3. 消费时机取出 → 批次处理：连续 User 段经 dispatch 管道（intercept 拦截 → 投影落库（seq 回填）→ 发送 → observe 观察），Control 条目先以 `OutputEvent::Control` 回显对外、后就地执行命令本体
4. pre-turn 压缩检查（上一轮真实 usage 对阈值门），命中则流式摘要 + 落 compaction 边界
5. `run_turn`：现查可见窗口组装请求 → `resolve_model` 路由 Provider → 流式 `StreamEvent` 聚合成 `Chunk` 事件出站
6. 流结束按 usage 计费落 assistant 消息；有 tool_calls 则编排执行（并行 / 串行），ToolResult 逐条出站，工具批完成后回 ReAct 顶部
7. 最终回复后 pending 倒灌 guide 再消费，双队列空才结束 turn
8. 全部事件经 fan-in 汇聚通道由 `App::recv()` 供上层 UI 消费；历史回放走 `messages_to_events` 反向投影，与实时流同构
