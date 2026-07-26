# API 参考

fuyao-core 的完整 API 参考由 rustdoc 自动生成，手写文档不抄录签名（避免双份维护过时）。

## 生成方式

```bash
cargo doc --workspace --no-deps --open
```

生成后用浏览器打开 `target/doc/fuyao_api/index.html`，或从各 crate 首页导航：

| crate | 文档入口 |
|-------|---------|
| fuyao-api | `target/doc/fuyao_api/` |
| fuyao-provider | `target/doc/fuyao_provider/` |
| fuyao-mcp | `target/doc/fuyao_mcp/` |
| fuyao-skills | `target/doc/fuyao_skills/` |
| fuyao-hooks | `target/doc/fuyao_hooks/` |
| fuyao-prompt | `target/doc/fuyao_prompt/` |
| fuyao-guard | `target/doc/fuyao_guard/` |
| fuyao-core | `target/doc/fuyao_core/` |
| fuyao-session | `target/doc/fuyao_session/` |
| fuyao-tools | `target/doc/fuyao_tools/` |
| fuyao-app | `target/doc/fuyao_app/` |

## 查什么用 rustdoc

- 公开结构体 / 枚举的字段与方法签名
- trait 的方法列表与实现者
- 类型继承与 re-export 关系
- 函数参数 / 返回值 / 错误类型

## 入口 API（最重要）

应用层绝大多数场景只用 `fuyao-app` 的几个入口 + `fuyao-core::Engine` 的方法，其它类型只在装配或扩展时才接触：

### 一行启动

```rust
use fuyao_api::AgentPaths;

let agent_paths = AgentPaths::default();
let (engine, app_ctx) = fuyao_app::start(agent_paths).await?;
```

### Engine 动作清单（五个交互 + 派生 + shutdown）

字段 / 签名细节见 rustdoc，核心动作清单如下（详见 [核心架构](../解释/核心架构.md)）：

| 动作 | 方法 | 入参 | 返回 |
|------|------|------|------|
| 启动引擎 | `Engine::new` | `EngineParams` / `ProviderRegistry` / `ToolRegistry` / `PluginHost` | `Engine` |
| 创建对话 | `engine.create_session` | `SessionParams` | `Result<SessionId, EngineError>` |
| 恢复对话 | `engine.resume_session` | `&SessionId` / `SessionParams` | `Result<(), EngineError>` |
| 入事件 | `engine.send` | `&SessionId` / `InputEvent` / `MessageParams` | `Result<(), EngineError>` |
| 出事件 | `engine.recv` | — | `Option<OutputEvent>` |
| 销毁单对话 | `engine.end_session` | `&SessionId` / `&str（end_reason）` | `Result<(), EngineError>` |
| 派生对话（fork） | `engine.fork_session` | `&SessionId`（源）/ `SessionParams` | `Result<SessionId, EngineError>`（`parent_session_id = None`） |
| 创建子任务 session | `engine.create_child_session` | `&SessionId`（父）/ `ChildSessionSource` / `SessionParams` | `Result<SessionId, EngineError>`（`parent_session_id = Some(父 id)`） |
| 关闭引擎 | `engine.shutdown` | — | `()` |

## 手写参考聚焦什么

手写参考文档（配置项、事件协议、crate 能力清单、错误类型）聚焦 rustdoc 做不了的：

- **配置文件语法**：`fuyao.toml` 怎么写、字段语义、合并规则
- **事件协议业务语义**：每个事件变体的触发时机、字段业务含义、跨变体关系
- **crate 间分层关系**：依赖方向、职责划分
- **错误类型触发条件**：每个变体在什么场景被触发、转换链
- **工具系统参数语义**：每个内置工具的 schema 与安全检查

永远不要把 rustdoc 内容复制进参考文档（会过时、双份维护）。
