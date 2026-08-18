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

应用层绝大多数场景只用 `fuyao-app` 的几个入口 + `fuyao-app::App` 的方法。`fuyao-core::Engine` 主要给装配层 / 测试 / 不走 `App` 装配的场景直接使用。

### 一行启动

```rust
use fuyao_api::{AgentPaths, EngineParams};

let fuyao = fuyao_app::start(EngineParams {
    agent_paths: AgentPaths::default(),
}).await?;
// fuyao: fuyao_app::FuyaoApp { app, sessions }
//   app: App         —— 运行时交互（create / send / stop / recv / end）
//   sessions: SessionManager —— 会话检索（list_sessions / session_count）
let app = fuyao.app;               // 跑对话
let sessions = fuyao.sessions;     // 查历史
```

`start` 串联 `init_engine`（配置 / 日志 / Provider）→ `build_tool_registry`（内置 + MCP 工具）→ 装配 `LoopGuardPlugin` → 创建 `SessionStore` → `Engine::new`（注入 store）→ `App::new` → `SessionManager::new`，返回 `FuyaoApp { app, sessions }`——`app` 与 `sessions` 共享同一份 `Arc<SessionStore>`，平级正交。签名见 rustdoc `fuyao_app` 首页。

### App 动作清单（应用层主入口）

`App` 包装 `Engine` 并承担 fan-in：每个**主 session**（`create_session` / `resume_session` / `fork_session`）创建时内部 spawn forwarder，把该 session 的 per-session 出站通道汇聚到单一 `fan_out` 通道，经 `App::recv` 对外暴露统一出口。签名细节见 rustdoc，动作清单如下（详见 [核心架构](../解释/核心架构.md)）：

| 动作 | 方法 | 入参 | 返回 |
|------|------|------|------|
| 启动 | `fuyao_app::start` | `EngineParams` | `Result<FuyaoApp { app, sessions }, SetupError>` |
| 创建对话 | `app.create_session` | `SessionParams` | `Result<SessionId, EngineError>`（rx 由内部 forwarder 消费进 fan_out） |
| 恢复对话 | `app.resume_session` | `&SessionId` / `SessionParams` | `Result<SessionId, EngineError>` |
| 派生对话（fork） | `app.fork_session` | `&SessionId`（源）/ `SessionParams` | `Result<SessionId, EngineError>`（`parent_session_id = None`，独立 session） |
| 创建子任务 session | `app.create_child_session` | `&SessionId`（父）/ `ChildSessionSource` / `SessionParams` | `Result<(SessionId, UnboundedReceiver<OutputEvent>), EngineError>`（rx **不进 fan_out**，返调用方独占消费） |
| 入事件 | `app.send` | `&SessionId` / `InputEvent` | `Result<(), EngineError>` |
| 停止会话 turn | `app.stop_session` | `&SessionId` / `&str（reason）` | `Result<(), EngineError>`（屏障语义：返回即该 session 在跑 turn 已完全终止、中断收尾落库已全部完成——DB 静默；session 保持存活，不写 ended_at） |
| 出事件 | `app.recv` | — | `Option<OutputEvent>`（单一出口，所有主 session 的事件汇聚于此） |
| 销毁单对话 | `app.end_session` | `&SessionId` / `&str（end_reason）` | `Result<(), EngineError>` |
| 关闭 | `app.shutdown` | 消费 `self` | `()`（两段式：engine.shutdown → forwarder 退出 → 停 MCP → drop） |
| 列历史会话 | `sessions.list_sessions` | `Option<&str>`（workspace 过滤）/ `i64` limit / `i64` offset | `Result<Vec<Session>, SessionError>`（按 `last_active_at` 倒序） |
| 会话总数 | `sessions.session_count` | `Option<&str>`（workspace 过滤） | `Result<i64, SessionError>` |
| 对话回退 | `sessions.rollback_session` | `&str（session_id）` / `i64（target_seq）` | `Result<RollbackPayload, SessionError>`（请求-响应直接返回载荷、不经事件流；安全顺序「先停后滚」——先 `app.stop_session` 屏障停 turn 再回退，两步之间无该 session 的并发写库） |

> **子 session 不进 fan_out**：子任务 session（`create_child_session` 产出）的 rx 直接返调用方独占消费——子代理 tool handler 用它取最终回复，fire-and-forget 后台任务 spawn 独立 task 消费。UI 出口只暴露主对话，避免子任务事件污染主对话流。

需要绕过 `App` 直接用 `Engine`（如自定义 fan-in / 测试）时，`Engine` 的 `create_session` / `resume_session` / `fork_session` / `create_child_session` 均返 `(SessionId, rx)`——rx 由调用方自行消费，详见 rustdoc。

## 手写参考聚焦什么

手写参考文档（配置项、事件协议、crate 能力清单、错误类型）聚焦 rustdoc 做不了的：

- **配置文件语法**：`fuyao.toml` 怎么写、字段语义、合并规则
- **事件协议业务语义**：每个事件变体的触发时机、字段业务含义、跨变体关系
- **crate 间分层关系**：依赖方向、职责划分
- **错误类型触发条件**：每个变体在什么场景被触发、转换链
- **工具系统参数语义**：每个内置工具的 schema 与安全检查

永远不要把 rustdoc 内容复制进参考文档（会过时、双份维护）。
