# fuyao.toml 全量配置参考

fuyao.toml 按三层加载与合并，本文给出全部配置段的字段、类型、默认值。

## 加载与合并规则

- 加载顺序 global → agent → workspace，最终优先级 **workspace > agent > global**
- **递归深合并**：双方同名键均为 table 时逐字段合并（如 workspace 层只覆盖 `[llm.retry]` 的一个字段，同表其他字段保留低层值）；标量与数组整值覆盖，**数组不跨层拼接**
- 三层文件全不存在时引擎以全默认值运行
- `[mcp_servers]` 段内所有 string 值支持 `${VAR}` 环境变量插值（未定义的变量保留字面量）
- `[providers]` 不参与跨层合并（只允许存在于全局层，见 `references/providers.md`）
- `.env` 环境变量文件与 fuyao.toml 同三层路径，逐层覆盖加载；仅启动时加载，运行期补载只补缺失、不覆盖既有

## 资源路径速查

| 资源 | global | agent | workspace |
| --- | --- | --- | --- |
| fuyao.toml | `~/.fuyao/fuyao.toml` | `{agent_root}/fuyao.toml` | `{工作目录}/.fuyao/fuyao.toml` |
| .env | `~/.fuyao/.env` | `{agent_root}/.env` | `{工作目录}/.fuyao/.env` |
| Agent 定义 | `~/.fuyao/agents/{名}.md` | `{agent_root}/agents/{名}.md` | `{工作目录}/.fuyao/agents/{名}.md` |
| skills | `~/.fuyao/skills/` | `{agent_root}/skills/` | `{工作目录}/.fuyao/skills/` |
| instructions | `~/.fuyao/instructions/` | `{agent_root}/instructions/` | `{工作目录}/.fuyao/instructions/` |
| AGENTS.md | `~/.fuyao/AGENTS.md` | `{agent_root}/AGENTS.md` | `{工作目录}/AGENTS.md`（在工作区根，不在 .fuyao 下） |
| sessions.db | `~/.fuyao/sessions/sessions.db`（无 agent_id 时） | `{agent_root}/sessions/sessions.db` | 不参与 |
| 日志 | `~/.fuyao/logs/`（无 agent_id 时） | `{agent_root}/logs/` | 不参与 |

agent_id 格式：`global/{名}` 或 `workspace/{名}`（来源前缀必须显式，大小写不敏感）；裸名或未知来源在引擎启动校验时报错。

## 配置段总览

| 段 | 用途 | 层级限制 |
| --- | --- | --- |
| `[models]` | 按用途标签选模型（轻量任务用 fast） | 三层均可 |
| `[providers]` | 供应商与模型定义 | **仅全局层**，字段见 `references/providers.md` |
| `[mcp_servers.{名}]` | MCP Server 连接定义 | 三层均可 |
| `[tools]` | 内置工具开关 / 并发策略 / 限额 / shell | 三层均可 |
| `[guard]` | 循环检测防护 | 三层均可 |
| `[llm]` | LLM HTTP 请求与重试 | 三层均可 |
| `[image]` | 图片压缩节流 | 三层均可 |
| `[session]` | 上下文压缩 / 存储 / 标题生成 | 三层均可 |
| `[mcp]` | MCP 全局 fallback 参数 | 三层均可 |
| `[engine]` | 引擎通道容量 | 三层均可 |
| `[hooks]` | 钩子执行超时 | 三层均可 |
| `[plugins]` | 插件开关 | 三层均可 |
| `[logging]` | 日志级别 / 轮转 / 控制台 | 三层均可 |

## 各段字段明细

### `[models]`

只认 `fast` 标签（配置其他标签加载报错）。主对话模型**不在此配置**——由创建会话时的模型参数显式指定；`fast` 未配置时轻量任务（标题生成、压缩总结）回退当前会话模型。

```toml
[models.fast]
model = "deepseek/deepseek-v4-flash"  # 格式：provider_id/model_id
thinking_type = "Enabled"             # Enabled / Disabled，缺省不发该字段
reasoning_effort = "high"             # 思考强度档位名，自定义字符串透传，缺省不发
```

### `[mcp_servers.{名}]`

每个条目一个 MCP Server，stdio 与 HTTP 二选一（配了 `url` 即 HTTP 传输）：

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `command` | string | 无 | stdio 启动命令 |
| `args` | array | 无 | stdio 命令参数 |
| `env` | table | 无 | stdio 环境变量 |
| `url` | string | 无 | HTTP endpoint |
| `headers` | table | 无 | HTTP 请求头 |
| `enabled` | bool | `true` | 是否启用 |
| `timeout` | 整数 | `120` | 工具调用超时（秒） |
| `connect_timeout` | 整数 | `60` | 连接超时（秒） |
| `tools` | table | `{}` | 工具开关（工具名 → bool），未列出默认启用 |

```toml
[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/path"]
env = { TOKEN = "${MCP_TOKEN}" }   # string 值支持 ${VAR} 插值
```

配好之后：

- 工具以 `mcp_{server名}_{工具名}` 注册进工具表（非安全字符替换为下划线），如 `mcp_filesystem_read_file`
- `tools` 过滤的 key 是 server 侧的**原始工具名**（不带 `mcp_` 前缀）
- server 启动部分失败不阻塞引擎，仅记 WARN 日志——「配了但工具没出现」先查日志文件（路径见下文 `[logging]` 节）

### `[tools]`

```toml
[tools.enabled]        # 工具总开关：工具名 → bool，未列出默认启用，显式 false 才禁用
bash = false

[tools.runner]
max_concurrent = 8                    # 最大并发工具数
never_parallel = ["bash", "todowrite"]           # 强制串行
parallel_safe = ["read", "glob", "grep", "skill", "webfetch", "subagent"]  # 可安全并行
path_scoped = ["read", "write", "edit"]          # 路径不重叠则并行，重叠串行

[tools.limits]
search_timeout_secs = 60              # 搜索命令超时
search_max_results = 500              # glob/grep 单次结果硬上限
terminal_default_timeout_secs = 120
terminal_max_timeout_secs = 6000
terminal_max_output_chars = 50000
webfetch_default_timeout_secs = 30
webfetch_max_timeout_secs = 180
webfetch_max_output_chars = 100000
webfetch_max_download_bytes = 5242880 # 5MB

[tools.terminal]
shell = "auto"   # auto / git_bash / powershell / cmd / bash / sh；未知值引擎启动时拒绝启动
```

内置工具名权威全集（`[tools.enabled]` 与 Agent 定义 frontmatter 的 `tools` 都用这些名字）：`read` / `write` / `edit` / `glob` / `grep` / `bash` / `webfetch` / `todowrite` / `skill` / `subagent`，另有 MCP 工具按 `mcp_{server}_{tool}` 命名。

两层过滤：全局 `[tools.enabled]` 在启动装配期生效（禁用即不注册）；Agent 定义 frontmatter 的 `tools` 在每个会话构建时生效，最终可用 = 两者交集。拼错的工具名静默忽略并记 WARN 日志，不报错。

并发判定顺序：never_parallel → path_scoped（路径重叠检查）→ parallel_safe。

### `[guard]`

TOML 键为 `[guard.loop]`（Rust 字段名 `loop` 是保留字经 rename 映射，TOML 键就是 `loop`）：

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `tool_repeat_threshold` | 整数 | `4` | 连续 N 次相同工具调用告警 |
| `tool_alternate_threshold` | 整数 | `6` | A→B→A→B 交替检测窗口 |
| `text_warn_threshold` | 浮点 | `0.6` | 文本重复警告线：末尾两窗口相似度超过即计入连续命中并发一次警告，回落即清零计数 |
| `text_interrupt_threshold` | 浮点 | `0.85` | 文本重复中断线：命中达此线（逐字重合复读特征）且连续命中数达标才中断 |
| `text_interrupt_hits` | 整数 | `3` | 中断要求的连续命中检查点数 |
| `streaming_check_interval` | 整数 | `100` | 流式检查间隔（字节） |
| `streaming_window_ratio` | 浮点 | `0.2` | 滑动窗口比例（比对累积文本末尾两个窗口） |

### `[llm]`

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `request_timeout_secs` | 整数 | `300` | HTTP 请求超时 |
| `connect_timeout_secs` | 整数 | `10` | HTTP 连接超时 |

```toml
[llm.retry]
initial_delay_ms = 2000          # 退避起始延迟
max_delay_ms = 30000             # 无响应头时退避上限
max_delay_with_headers_ms = 2147483647  # 有响应头时退避上限
max_retries = 4294967295         # 可恢复错误最大重试次数（默认无限，u32 上限）
```

严重错误（认证失败 / 4xx 等）不重试，与 `max_retries` 无关。

### `[image]`

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `compress` | bool | `true` | 超限图是否压缩节流 |
| `max_pixels` | 整数 | `2000` | 最大边长（像素），超限等比缩放 |
| `max_base64_bytes` | 整数 | `5242880` | base64 字节上限 |
| `jpeg_qualities` | array | `[85, 80, 70, 55, 40]` | JPEG 质量档位（降序尝试） |

### `[session]`

```toml
[session.compression]
enabled = true            # 启用上下文压缩
threshold = 0.85          # 触发阈值：prompt_tokens >= threshold × (context_length - summary_max_tokens)
summary_max_tokens = 4096 # 摘要输出上限
skip_child = true         # 子 session 不自动压缩

[session.storage]
busy_timeout_secs = 5
max_connections = 5

[session.title]
enabled = true
snippet_max_chars = 500
max_len = 80
```

### `[mcp]`

MCP 全局 fallback（单个 server 配置未写时取这里的值）：

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `tool_timeout_secs` | 整数 | `120` | 默认工具调用超时 |
| `connect_timeout_secs` | 整数 | `60` | 默认连接超时 |
| `max_backoff_secs` | 整数 | `60` | 最大退避 |
| `max_reconnect_retries` | 整数 | `5` | 最大重连次数 |
| `max_initial_connect_retries` | 整数 | `3` | 最大初始连接重试 |
| `circuit_breaker_threshold` | 整数 | `3` | 熔断触发阈值（连续失败） |
| `circuit_breaker_cooldown_secs` | 整数 | `60` | 熔断冷却 |
| `session_recovery_wait_secs` | 整数 | `15` | Session 恢复等待 |

### `[engine]`

引擎运行时通道容量，三个字段与实际创建通道一一对应：

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `inbound_channel_capacity` | 整数 | `32` | session 级统一入站通道容量（User / Control / 插件注入条目共用，保证总序） |
| `interrupt_channel_capacity` | 整数 | `8` | session 级中断通道容量（中断信号量小且瞬时） |
| `fan_out_capacity` | 整数 | `512` | 进程级 fan-out 汇聚通道容量：全部 session 事件汇聚单出口的缓冲上限，是引擎唯一的背压点 |

修改容量只影响之后创建的通道。per-session 出站通道刻意无界（不设容量字段）：事件入通道前已落库，无界保证不因上层消费慢而反压 ReAct 推进——背压统一放在 fan-out。

### `[hooks]`

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `timeout_secs` | 整数 | `5` | 单个 hook 执行超时；`0` 表示不超时（慎用） |

本段只配置钩子的**执行参数**。钩子本身属开发面：由插件在启动装配期经代码注册（插件开关见 `[plugins]`），配置文件中不存在「定义 / 注册钩子」的写法。

### `[plugins]`

```toml
[plugins.enabled]   # key = 插件名，未列出默认启用，显式 false 才禁用
loop_guard = false
```

内置插件：`loop_guard`（循环检测防护）。未知的插件名记 WARN 后忽略。插件在启动装配期注册，改配置需重启引擎生效。

### `[logging]`

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `level` | string | `"info"` | EnvFilter 指令语法（如 `"fuyao_provider=warn"`）；运行时 `RUST_LOG` 环境变量优先覆盖 |
| `console` | bool | `true` | 是否同时输出 stderr（文件层始终输出） |
| `rotation` | string | `"daily"` | `daily` / `hourly` / `never`，非法值解析报错 |

日志文件按 agent 落盘：有 agent_id 时在 `{agent_root}/logs/`，否则 `~/.fuyao/logs/`。

## 配置生效方式

- 除 `[providers]` 外的所有段：引擎启动时一次性加载并注入全局只读句柄，**修改后需重启引擎重新装配才生效**；多引擎进程内共享首份配置
- `[providers]` 例外：应用层可调用热刷新（重新三层加载 → 逐个注册覆盖 → 清理已删除项），存活引擎下一 turn 生效；无热刷新入口时同样需重启
- `.env`：启动期三层覆盖加载；运行期只补缺失变量，不覆盖既有值
