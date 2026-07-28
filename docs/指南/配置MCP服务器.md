# 配置 MCP 服务器

> stdio 传输（本地进程）或 HTTP 传输（远程服务），配 fuyao.toml 的 `[mcp_servers.*]`。
>
> MCP 工具是**引擎级共享**：启动时由 `fuyao-app::build_tool_registry` 收集成 `ToolEntry` 注入 `ToolRegistry`，所有 session 共享同一份工具表。

## stdio 传输（本地进程）

```toml
[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
enabled = true
timeout = 120
```

启动时 `MCPManager.start_all` 自动拉起子进程，发现工具并注册（经 `get_tool_entries` 收集进 `ToolRegistry`）。

## HTTP 传输（远程服务）

```toml
[mcp_servers.remote]
url = "https://${MCP_HOST}/mcp"
headers = { Authorization = "Bearer ${MCP_TOKEN}" }
enabled = true
```

所有 string 值支持 `${VAR}` 环境变量插值（从 .env 读取）。

## 工具开关

按工具名关闭特定 MCP 工具：

```toml
[mcp_servers.filesystem.tools]
write_file = false
```

未列出的工具默认启用。

## 全局 fallback

`[mcp]` 段提供 per-server 未配时的默认值：

```toml
[mcp]
tool_timeout_secs = 120
connect_timeout_secs = 60
circuit_breaker_threshold = 3
```

## 生命周期管理

`MCPManager` 由 `App` 持有（不在 Engine 内——`fuyao-core` 不依赖 `fuyao-mcp`）。应用退出时调 `App::shutdown` 串联「先关 MCP 再退出」：

```rust
let app = fuyao_app::start(EngineParams { agent_paths }).await?;

// ... 使用 app ...

// 优雅停机（App::shutdown 内部串联）
//   1. engine.shutdown()：所有 session task 落库退出 → forwarder 退出
//   2. mcp_manager.stop_all()：每个 MCP 连接走 rmcp close_with_timeout 优雅关闭
//                              （先关 transport 让 server 退出、超时 kill 子进程）
app.shutdown().await;
```

## 验证

```rust
let app = fuyao_app::start(EngineParams { agent_paths }).await?;

// MCPManager 由 App 内部持有；此处用 App::recv 验证工具可调
// （若需直接查 server 状态，自行调 init_engine + build_tool_registry 拿 MCPManager）

// 让 LLM 调用 MCP 工具
let session_id = app.create_session(SessionParams::default()).await?;
app.send(&session_id, InputEvent::User(msg)).await?;
// 观察事件流：ToolCall(tool_name="mcp_filesystem_read_file") → ToolResult → ...
```

## 容错能力（自动启用）

经 `[mcp_servers]` 配置的 MCP 工具调用走**四层防护**（无需配置，handler 内置）：

- **超时保护**：每个工具调用 `tokio::time::timeout`（用 server 级 `timeout` 或 `[mcp]` fallback）
- **熔断器**：连续失败达 `circuit_breaker_threshold` 触发熔断，冷却期间直接返错
- **Auth 恢复**：401/403 触发 `notify_reconnect` + `wait_and_retry`
- **Session 恢复**：session expired 触发自动重连

> 主动健康检查不做（参考项目都没有，被动重连 + 熔断够用）。详见 [MCP 系统设计](../解释/MCP系统设计.md)。

## 相关

- [MCP 系统设计](../解释/MCP系统设计.md) — 生命周期、bridge、熔断、重连、安全防护
- [配置项参考](../参考/配置项参考.md) — `[mcp_servers.*]` 和 `[mcp]` 全字段
- [crate 能力清单](../参考/crate能力清单.md) — MCPManager API
