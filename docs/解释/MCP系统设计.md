# MCP 系统设计

> 本文解释 MCP 集成的生命周期管理、工具桥接、熔断恢复与安全防护。API 签名见 `cargo doc --workspace`；配置见 [配置 MCP 服务器](../指南/配置MCP服务器.md)。

> **多 session 架构下的 MCP**：MCP 工具是**引擎级共享**——启动时由 `fuyao-app::build_tool_registry` 收集成 `ToolEntry` 注入 `ToolRegistry`，所有 session 共享同一份工具表（详见 [工具系统设计](工具系统设计.md)）。MCPManager 由 `App` 持有保活，与 `Engine` 平级（不在 engine 内）。

## 架构分层

```text
MCPManager（lib.rs）       ← 编排器：管理多 Server 生命周期、工具发现 / 注册 / 调用
  │
  ├── bridge.rs            ← 适配层：MCP 工具 → ToolEntry（中立工具定义 + handler）
  │     └── 四层防护：超时 + 熔断 + Auth 恢复 + Session 恢复
  │
  ├── connection.rs        ← 传输层：MCPConnection 封装单 Server 连接（stdio / HTTP）
  │
  ├── circuit_breaker.rs   ← 熔断器（全局静态）
  ├── recovery.rs          ← Auth / Session 过期检测
  └── security.rs          ← env 过滤 + 错误脱敏 + schema 标准化 + 名称清洗
```

底层使用官方 `rmcp`（Rust MCP SDK）驱动连接。

## MCPManager 生命周期

| 方法 | 说明 |
|------|------|
| `from_config()` | 从 `[mcp_servers]` 配置创建管理器 |
| `start_all()` | 批量启动，单个失败不阻塞其他，返回 (成功数, 失败数, 失败详情) |
| `stop_all()` | 断开所有连接 + 清空工具（每个连接执行 rmcp 的 `close_with_timeout` 优雅关闭） |
| `get_tool_entries()` | 直返 `Vec<ToolEntry>`（schema + handler + 可见性），供 `fuyao-app::build_tool_registry` 零转换收集进 `ToolRegistry` |
| `disconnect()` | 单连接优雅关闭（用 rmcp 的 `close_with_timeout`：先关 transport 让 server 退出、超时 kill 子进程） |

工具发现是连接的副产物——连接成功即调 `list_all_tools()` 拉取工具列表，不需要额外步骤。

### 装配流程（fuyao-app）

```text
build_tool_registry():
  1. collect_builtin_tools()                ← 内置工具（按 [tools.enabled] 过滤）
  2. collect_mcp_tools()                    ← MCP 工具（有配置时 start_all + get_tool_entries 直返 ToolEntry）
     │   └─ 工具发现时（连接的副产物）已把 server 的 JSON schema 转成强类型 ToolDefinition，装配层零序列化往返
  3. ToolRegistryBuilder::register_all(...) ← 全部注入
  4. 返回 (ToolRegistry, Option<Arc<MCPManager>>)
```

`App` 持有 `Option<Arc<MCPManager>>` 保活，应用退出时调 `App::shutdown()` 实现「先关 MCP 再退出」的有序停机。

## 双传输（stdio / HTTP）

由配置判定：有 `url` → HTTP，有 `command` → stdio。

| 传输 | 特点 |
|------|------|
| stdio | `tokio::process::Command` 启动子进程，环境变量经 `build_safe_env` 白名单过滤 |
| HTTP | `StreamableHttpClientTransport`，`reinit_on_expired_session(true)` 自动续期，支持自定义 headers |

连接由后台 Task 维护长连接循环，处理重连信号。

## 工具桥接（bridge）

### 名称前缀化

```text
build_prefixed_name("github", "search") → "mcp_github_search"
```

格式 `mcp_{server}_{tool}`：命名空间隔离（不同 Server 可能有同名工具）+ 来源可识别 + 与内置工具区分。非字母数字下划线字符清洗为 `_`。

### RegisteredTool

```text
RegisteredTool {
    prefixed_name: String    // 对外唯一标识（LLM 看到的名字）
    original_name: String    // 调用 MCP Server 时用的名字
    server_name: String      // 反查连接的 key
    description: String      // 工具描述
    schema: ToolDefinition   // 工具定义（中立三要素，供应商适配层再编码为 wire 形态）
}
```

对外用前缀名（防冲突），对内用原始名（Server 只认自己的名字）。

### 为什么用 Arc\<Mutex\<Option\<MCPConnection\>\>\> 传递连接？

`ToolFn` 要求 `'static + Send + Sync`，handler 需要运行时访问连接。这是 Rust 异步代码中表达「共享、可变、可空」状态的经典三件套：Arc（共享所有权）+ Mutex（并发串行化）+ Option（连接可能断开）。

## 四层调用防护

经 `make_tool_call_handler` 注册到引擎的工具，每次调用都走四层防护：

```text
handler(args, ctx, cancel)
  │
  ├─ 1. check_breaker(server) ─── 熔断中？直接返回错误信封 {"error": msg}
  │
  ├─ 2. tokio::time::timeout ──── 超时保护
  │
  ├─ 3. do_call ─── 实际 MCP 调用（结果统一装进 ToolOutput 信封）
  │    │
  │    └── 失败时：
  │        ├─ is_auth_error → notify_reconnect + wait_and_retry
  │        ├─ is_session_expired → notify_reconnect + wait_and_retry
  │        └── 其他 → bump_error + 返回脱敏错误
  │
  └─ 4. 结果处理（信封类型判定 is_error 驱动熔断计数 + text/structured 提取 + sanitize_error 脱敏）
```

> **双路径行为差异**（重要）：
> - `get_tool_entries()` 产出的 handler（LLM 触发路径）：**四层全防护**——超时 + 熔断 + Auth 恢复 + Session 恢复
> - `MCPManager::call_tool()`（程序化直调路径）：**无防护**——无超时、无熔断、无 Auth/Session 恢复
>
> 两条路径调用同一底层 `conn.call_tool`，但容错能力截然不同。Engine 走 handler 路径（有保护），外部代码直接调 `call_tool` 则裸奔。

## 熔断器（隐式三态）

用计数 + 时间隐式表达三态（无显式枚举）：

| 状态 | 判定 | check_breaker |
|------|------|--------------|
| Closed（正常） | count < threshold（默认 3） | 放行 |
| Open（熔断） | count ≥ threshold 且冷却未过（默认 60s） | 拦截 |
| Half-open（试探） | count ≥ threshold 但冷却已过 | 放行 |

- `bump_error`：失败 +1，达阈值记录 opened_at
- `reset_error`：成功后清零，回到 Closed
- Half-open 是乐观的——直接放行，无并发试探限制

## 重连恢复

| 场景 | 重试上限 | 说明 |
|------|---------|------|
| 初始连接失败 | 3 次 | 给 Server 启动缓冲 |
| 运行中断开 | 5 次 | 有限重连，耗尽即放弃 |

指数退避（1s 起步，封顶 60s）。Auth / Session 过期错误触发 `notify_reconnect`（唤醒后台 Task 重连）+ `wait_and_retry`（等 15s 后重试一次）。

## 安全防护

| 防线 | 机制 | 防什么 |
|------|------|--------|
| env 白名单 | `build_safe_env` 只传白名单 + 用户显式配置 | API Key 泄露给子进程 |
| 错误脱敏 | `sanitize_error` 正则替换凭证为 `[REDACTED]` | 凭证进入 LLM 上下文 |
| schema 标准化 | `normalize_mcp_input_schema` 四步处理 | 格式不兼容导致 LLM 拒收 |
| 名称清洗 | `sanitize_mcp_name_component` | 工具名含非标识符字符（连字符 / 点号等） |

schema 标准化四步：`definitions` → `$defs` / 折叠 nullable union / 补 object 形状 / 裁剪悬空 required。

## 关键设计决策

### 为什么工具发现是连接的副产物？

简化流程。连接成功后立即 `list_all_tools()`，连接 = 发现，不需要分两步。失败时连接本身就失败了，不会有"连接成功但发现失败"的中间态。

### 为什么熔断器用计数+时间而非显式枚举？

少一个状态机要维护。计数 + 时间足以表达三态语义，且代码更简洁。代价是 half-open 无并发试探限制——但对 MCP 工具调用（通常低并发）这个代价可接受。

### 为什么错误要脱敏？

MCP Server 返回的错误可能回显其配置（连接字符串含密码、Authorization header）。这些信息进入 LLM 上下文或日志后有泄露风险。`sanitize_error` 在所有返回路径拦截。
