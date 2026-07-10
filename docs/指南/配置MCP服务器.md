# 配置 MCP 服务器

> stdio 传输（本地进程）或 HTTP 传输（远程服务），配 fuyao.toml 的 `[mcp_servers.*]`。

## stdio 传输（本地进程）

```toml
[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
enabled = true
timeout = 120
```

启动时 MCPManager 自动拉起子进程，发现工具并注册。

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

## 验证

```rust
let (_, handle, app_ctx) = fuyao_app::start(agent_ctx).await?;
// app_ctx.mcp_manager.get_server_status() 查看 Server 连接状态
// handle.send_message 让 LLM 调用 MCP 工具
```

## 相关

- [配置项参考](../参考/配置项参考.md) — `[mcp_servers.*]` 和 `[mcp]` 全字段
- [crate 能力清单](../参考/crate能力清单.md) — MCPManager API
