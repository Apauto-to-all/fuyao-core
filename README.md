# Fuyao-core

轻量级 Agent 引擎 SDK，负责单个 Agent 的生命周期：ReAct 循环、会话、工具与 LLM 调用。

[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2024_edition-orange.svg)](Cargo.toml)

## 特性

- **ReAct 循环引擎**：每 session 一个 tokio task，想 → 调一批工具 → 消费队列 → 再想 → 最终回复
- **多供应商路由**：`model_id` 形如 `provider_id/model_id`，按前缀拆解路由，支持运行时注册 / 注销
- **OpenAI + Anthropic 兼容协议**：`openai-completions` / `anthropic-messages` 两种 wire 方言
- **MCP 集成**：MCP Server 连接管理 + 工具发现 / 注册 / 调用，实例级熔断
- **Agent Skills 协议**：三层渐进披露（元数据发现 → 完整内容 → 关联文件按需加载）
- **会话持久化**：SQLite（WAL），消息 / 待办 / 文件快照四表，上下文压缩与费用计算
- **文件快照**：影子 git 仓（独立 git-dir），工具批边界与 turn 收尾采集，按基线树恢复，用户 `.git` 永不被触碰
- **钩子与插件**：拦截钩子（同步原地修改）+ 观察钩子（异步只读）+ 内置循环防护插件
- **10 个内置工具**：`read` / `write` / `glob` / `grep` / `edit` / `bash` / `skill` / `subagent` / `todowrite` / `webfetch`
- **子代理**：派生 child session 跑专职定义，最终回复回喂父循环

## 架构

11 个 crate 依赖严格单向，按职责分五类：

| 类别 | crate | 职责 |
| --- | --- | --- |
| 基座 | `fuyao-api` | 公共类型（trait / 配置 / 路径 / 消息 / 事件协议），零内部依赖 |
| 引擎内核 | `fuyao-core` | ReAct 循环 + dispatch 统一消息管道 + 多 session 调度 |
| 内核协作者 | `fuyao-session` / `fuyao-prompt` / `fuyao-hooks` | 会话持久化 / 提示词分层与 Agent 定义 / 钩子系统 |
| 能力实现 | `fuyao-provider` / `fuyao-mcp` / `fuyao-tools` / `fuyao-guard` / `fuyao-snapshot` | LLM 客户端 / MCP 连接 / 内置工具 / 防护插件 / 文件快照 |
| 装配入口 | `fuyao-app` | 一键装配（init → 收集工具 → 启动引擎），应用层唯一依赖 |

## 文档

- `docs/adr/`：架构决策记录
- `CONTEXT.md`：领域术语表与关键不变量

## License

Apache-2.0，详见 [LICENSE](LICENSE)。
