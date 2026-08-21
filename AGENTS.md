# Fuyao-core 技术栈

**语言**：Rust 2024 edition
**LLM 交互**：自建 LLM 客户端，支持 OpenAI 兼容接口
**配置格式**：TOML
**MCP 集成**：MCP 协议，使用官方 Rust MCP SDK
**Skills**：Agent Skills 协议（SKILL.md + frontmatter）

## 系统定位

fuyao-core 是**独立 Agent 引擎 SDK**——配置好模型就能跑的独立 Agent。

- **单 Agent 范畴**：核心只负责单个 Agent 的生命周期（ReAct 循环、会话、工具、LLM 调用）
- **瘦身**：这个是核心，避免添加过多第三方依赖，保持轻量级；且功能必须简洁，避免繁琐。

## 项目导航

所有源码在 `crates/` 下，11 个 crate 依赖严格单向，禁止反向依赖。按职责分五类：

**基座**（公共类型）：

- `fuyao-api`：公共类型（trait / 配置 / 路径 / 消息 / 事件协议），零内部依赖，所有 crate 的地基

**引擎内核**（ReAct 循环 + session 管理）：

- `fuyao-core`：ReAct 循环 + dispatch 统一消息管道 + 多 session 调度，对外暴露四个动作（启动 / 创建 / 恢复 / 收发）。session 化双层架构的「能力共享层」，持有 provider / DB 句柄 / 出口通道

**内核协作者**（被 core 直接依赖 + 被能力层复用，构成内核但不参与 ReAct 编排）：

- `fuyao-session`：会话持久化（SQLite CRUD）+ 上下文压缩 + 费用计算 + 标题生成
- `fuyao-prompt`：系统提示词分层构建（覆盖区 + 补充区）+ Agent 定义加载与注册表 + 内置默认子代理定义。**被 fuyao-core 与 fuyao-tools 双重消费**（fuyao-tools 的 subagent/skill 工具调其定义加载与列表查询能力）
- `fuyao-hooks`：钩子系统（拦截 + 观察）+ 插件两层模型（Plugin 工厂 / PluginInstance 实例）。**被 fuyao-core 与 fuyao-guard 双重消费**

**能力实现**（引擎装配的能力，多数彼此独立、可增删替换；个别有跨层复用见各条说明）：

- `fuyao-provider`：LLM 客户端（自建 HTTP，OpenAI 兼容）+ ProviderRegistry 多路由
- `fuyao-mcp`：MCP Server 连接管理 + 工具发现 / 注册 / 调用
- `fuyao-skills`：Agent Skills 协议（发现 / 加载 / 解析）
- `fuyao-tools`：内置工具实现集合（file / terminal / web / todo / skill / subagent）。**依赖 fuyao-prompt（子代理定义加载与校验、Agent 定义查询）+ fuyao-skills（skill 工具的发现能力）**
- `fuyao-guard`：内置防护插件（循环检测，防重复执行 / 输出），基于 hooks 插件机制接入。**依赖 fuyao-hooks**

**装配入口**（项目唯一的组装点，把下层能力装配成可用的引擎）：

- `fuyao-app`：一键装配入口（init → 收集工具 → 启动引擎），应用层（cli / tui）的唯一依赖

### 文档导航

开发前按下表查阅对应文档（已覆盖架构设计、公开 API、配置项、事件协议等）：

| 目录 | 定位 | 查什么 |
| ------ | ------ | ------ |
| `docs/参考/` | 查事实 | crate 公开 API → `crate能力清单.md`；配置项 → `配置项参考.md`；事件 → `事件协议参考.md` |
| `docs/解释/` | 理解设计 | 架构全貌 → `核心架构.md`；引擎 → `引擎内核设计.md`；钩子 → `Hooks与Plugin设计.md` |
| `docs/指南/` | 怎么做某件事 | 按需查阅 |
| `docs/教程/` | 教程 | 按需查阅 |

**阅读原则**：先读 `docs/解释/` 理解设计背景，再到 `docs/参考/` 查具体字段，文档不足时才深入 `crates/` 源码。改架构前尤其要先读对应子系统的设计文档；文档已有的信息禁止重新扫描源码。

## 架构约定

- **消息类型只认输出侧**：`InputEvent` 在入口即转成 `OutputEvent`，内核不引入输入侧类型、不区分消息方向
- **消息驱动架构**：项目以 `OutputEvent` 表达一切对外状态变化——开发时遇新生命周期信号，首选加事件变体，而非给现有 payload 挂额外字段
- **引擎是忠实执行器**：忠实触发外部一切命令，不做去重 / 合并 / 冷却等「意图解释」——那属于上层职责（如按钮防抖、调用方自查）。引擎避免有自己的想法：收到几条就执行几次。

## 工具设计原则

- **自主决策**：返回足够信息让 Agent 自行决策，错误信息含明确建议，避免"询问用户"类工具
- **容错降级**：文件不存在时建议相似文件，匹配失败尝试模糊策略，敏感操作返回明确错误但提供替代方案
- **安全边界**：禁止写入敏感路径、禁止读取无限输出设备、权限错误必须明确告知
- **长时间运行**：工具无状态或状态可恢复，支持分页/断点续传

### 注意事项

项目中存在两个「Agent」相关概念，**分属不同维度、不可混为一谈**：

| 概念 | 本质 | 存储位置 | 决定什么 |
| ------ | ------ | --------- | --------- |
| **agent_id** | 独立 Agent 实体（数据隔离单元） | `fuyao-agents/{id}/`（一个目录） | 数据在哪：sessions.db、Provider 配置、缓存 |
| **Agent 定义** | 定义提示词（一项能力）；终端用户称「智能体」 | 四层 `agents/{name}.md`（见下） | 内容是什么：系统提示词、行为指令 |

- **agent_id** 是路径参数：`global/{名}` / `workspace/{名}`（来源前缀必须显式，大小写不敏感；裸名 / 未知来源在引擎启动校验时报错，禁止隐式选址）
- **Agent 定义** 由 `AgentConfig.definition` 选择（None 时取 `"default"`），按四层优先级解析同名 `.md`：workspace > agent > global > extra，外加内置 `default`/`explore`/`executor` 兜底
- **agent 层**：提供 agent_id 时，`{agent_root}/agents/{name}.md` 作为定义来源之一——独立 Agent 可携带私有定义（含私有 `default`），覆盖同名共享定义
- **任意组合**：任何 agent_id 仍可经 `AgentConfig.definition` 选择任何具名定义（如 agent_id=`coder` 用 `reviewer`）；agent 层只是额外来源，不限制组合自由

## 开发规范

### 通用规范

- 注释必须保留或更完善：禁止删除原有注释，新代码必须添加完整注释
- 注释只描述代码本身：禁止在代码注释里出现任何"出处/对标"话语——不论动词（参考/对齐/学/借鉴/移植/搬自/对应…，不穷举）其他文件、文档、项目、链接等干扰话语。设计依据要写就重述为代码自身的设计陈述，不带出处。改完 .rs 必须自检注释是否命中禁项。
- 发送给 LLM 的所有文本必须使用中文，包括提示词、标签、占位符
- 目前是开发阶段，禁止做任何兼容处理，对于废弃的代码，直接删除，禁止进行兼容保留
- 项目干净：不被使用的函数、多余的导出，**先判断**是否有价值，如果有就保留，并且用 `// TODO:` 标记并说明用途，如果该代码目前不使用，可以使用 `//` 把代码完整注释掉，便于后续直接启用。对于废弃代码，无用代码，死代码，直接删除
- 模型唯一示例：deepseek/deepseek-v4-flash

### Rust 开发规范

项目采用 **Workspace 依赖统一管理**，所有依赖版本必须在根 `Cargo.toml` 的 `[workspace.dependencies]` 中声明，子 crate 通过 `workspace = true` 引用，禁止在子 crate 中硬编码版本号。

开发 Rust 前，必须加载：rust-patterns 和 rust-testing skills；开发修改rust文件后，需要使用 `cargo fmt` 格式化代码

- 搜索最新版本：`cargo search <包名> --limit 1`，获取最新版本号
- 手动编辑根 `Cargo.toml`，在 `[workspace.dependencies]` 中添加依赖及版本号（使用搜索得到的最新版本）
- 在需要该依赖的子 crate 中引用：`cargo add <包名> -p <crate名>`
  - 如果该依赖已在 `[workspace.dependencies]` 中，此命令会自动使用 `workspace = true` 形式
  - 如果该依赖不在 workspace 中，此命令会直接在子 crate 中硬编码版本号（**禁止**）

**禁止使用过时版本**：每次添加新依赖必须先搜索 crates.io 确认最新版本，禁止凭记忆或使用旧版本号。

#### 常用命令（Cargo）

- 运行检查：`cargo check`（比 build 快，推荐开发时使用）
- 运行测试：`cargo test`
- 格式化代码：`cargo fmt`
- 静态检查：`cargo clippy -- -D warnings`（仅生产代码，开发期快检）
- 交付前全检：`cargo clippy --workspace --tests -- -D warnings`（含 `#[cfg(test)]` 测试代码，阶段性交付 / 提交前必跑）
- 覆盖率：`cargo llvm-cov --fail-under-lines 80`
- 创建各个 crate： `cargo init --lib crates/fuyao-xxx`       # lib crate

#### 测试规范（Cargo）

- **开发过程避免跑全量测试**：`cargo test --workspace`（尤其含集成测试）耗时长，**除非用户明确要求**，否则开发过程中不要主动运行。开发期验证优先用 `cargo check`（快）和 `cargo clippy -- -D warnings`（静态检查）；确需跑测试时，用 `cargo test -p <crate>` 限定单个 crate 或 `cargo test -p <crate> --lib` 只跑单元测试。完整测试仅在阶段性交付、提交前、或用户要求时运行。
- **交付前 clippy 必含 `--tests`**：`cargo check` 和不带 `--tests` 的 `cargo clippy` 都**不编译 `#[cfg(test)]` 测试代码**，测试里的 clippy lint 与漏参 bug 只有加 `--tests` 才会暴露。阶段性交付 / 提交前，务必跑 `cargo clippy --workspace --tests -- -D warnings`。
- 运行测试：`cargo test --workspace`（包含所有 crate 的测试）
- 单元测试：在源文件内使用 `#[cfg(test)] mod tests { ... }`
- 集成测试：放在 `tests/` 目录下，每个文件是独立的测试二进制
- 编写或修改集成测试（`tests/` 目录）前，必须加载 `fuyao-integration-test` skill
- 异步测试：使用 `#[tokio::test]`
- 覆盖率目标：关键业务逻辑 100%，公共 API 90%+，通用代码 80%+
- rust 项目需要在源码实现单元测试

### 日志规范

日志地基（tracing subscriber + `[logging]` 配置 + per-agent 文件 + guard）已落地，所有 crate 写日志须遵循以下规范。

**三条铁律**：① 事件而非流（一条日志 = 一个有意义事件，不是每行都记）② 结构化（字段 `key=value`，禁止拼进消息）③ 5W 齐全（时间戳 / target / 消息 / 字段 / 结果）。

**级别语义**：

| 级别 | 用途 | 典型场景 |
| ------ | ------ | --------- |
| ERROR | 需人介入，功能受损 | Provider 创建失败、MCP 启动失败 |
| WARN | 可疑 / 可恢复 | LLM 重试、MCP 熔断、压缩触发、插件 panic 恢复 |
| INFO | 业务里程碑 / 外部交互 / 决策 | 引擎初始化、LLM 耗时、工具调用、会话创建 |
| DEBUG | 诊断细节 | 流式 chunk、中间状态 |
| TRACE | 极细粒度 | 一般不用 |

**两条黄金法则**：① 半夜叫醒测试——只有 ERROR 触发报警，可恢复失败一律 WARN；② INFO 重建故事——读 INFO 能重建系统做了什么，翻屏说明把 DEBUG 误当 INFO。

**字段命名**：耗时 `elapsed_ms`、计数明确名词（`attempt`/`turn`）、布尔形容词（`ok`/`recovered`）、错误原因用 `cause`（非 `error`），统一 `snake_case`。消息用中文陈述句。

**敏感信息红线**：禁止记录 API Key / Token / 用户输入全文 / 文件内容 / 含密钥命令。错误信息含密钥需脱敏。

**高频热路径不记 INFO**：流式 chunk / 循环每次迭代用 DEBUG 或不记。
