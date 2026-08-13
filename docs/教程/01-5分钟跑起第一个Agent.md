# 5 分钟跑起第一个 Agent

> 本教程带你从零创建一个能对话的 Agent。完成后你将理解配置、启动、消息收发的完整流程。

## 前置条件

- Rust 工具链（cargo）
- 一个 OpenAI 兼容的 API Key（如 DeepSeek）

## 第 1 步：配置 fuyao.toml

在项目根创建 `fuyao.toml`：

```toml
[providers.deepseek]
name = "DeepSeek"
api_key_env_vars = ["DEEPSEEK_API_KEY"]

[providers.deepseek.options]
base_url = "https://api.deepseek.com/v1"

[providers.deepseek.models."deepseek-v4-flash"]
name = "deepseek-v4-flash"
```

**预期结果**：配置文件创建成功，指定了供应商和模型（主对话模型在创建会话时显式提供，不在 fuyao.toml 配置）。

## 第 2 步：配置 .env

在项目根创建 `.env`：

```text
DEEPSEEK_API_KEY=sk-你的密钥
```

**预期结果**：环境变量文件创建成功。

## 第 3 步：启动 Agent 并对话

在你的项目中添加依赖并启动。引擎支持**多 session 并发**——必须先创建对话拿编号，再发消息：

```rust
use fuyao_api::{AgentPaths, EngineParams, InputEvent, ModelConfig, OutputEvent, SessionParams};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. 构造 AgentPaths（默认使用全局层路径）
    let agent_paths = AgentPaths::default();

    // 2. 一行启动：初始化引擎 + 收集工具 + 装配插件 + 日志 + App fan-in
    //    内部完成：init_engine → build_tool_registry → Engine::new → App::new
    let app = fuyao_app::start(EngineParams { agent_paths: agent_paths.clone() }).await?;

    println!("Agent 启动成功！");

    // 3. 创建对话拿编号（每个对话有独立 session_id）
    //    主对话模型在此时显式提供（ModelConfig.model_id，String 必填），引擎不提供隐式兜底
    let session_id = app.create_session(SessionParams {
        model_config: ModelConfig {
            model_id: "deepseek/deepseek-v4-flash".to_string(),
            ..Default::default()
        },
        ..Default::default()
    }).await?;

    // 4. 发送一条用户消息（模型由 session 的 SessionParams.model_config 决定）
    use fuyao_api::message::input::{UserMessage, UserPayload};
    use fuyao_api::message::{EventBase, UserMessageMode, UserMessageSource};
    let user_msg = UserMessage {
        base: EventBase::default(),
        payload: UserPayload {
            content: "你好，介绍一下你自己".to_string(),
            mode: UserMessageMode::Guide,
            source: UserMessageSource::User,
        },
    };
    app.send(&session_id, InputEvent::User(user_msg)).await?;

    // 5. 接收流式响应（所有主 session 的产出经 fan-in 从 App::recv 单一出口流出）
    while let Some(event) = app.recv().await {
        // 按事件 base.session_id 归类到对应对话（多 session 并发不串）
        match event {
            OutputEvent::Chunk(c) => {
                if let Some(text) = c.payload.content {
                    print!("{}", text);
                }
            }
            OutputEvent::Assistant(_) => {
                println!("\n--- 回复完成 ---");
                break;
            }
            _ => {}
        }
    }

    // 6. 优雅关闭（先停引擎再关 MCP）
    app.shutdown().await;
    Ok(())
}
```

**预期结果**：终端输出 `Agent 启动成功！`，然后流式打印 AI 的回复，最后显示 `--- 回复完成 ---`。

## 发生了什么？

1. `AgentPaths::default()` 创建默认路径（全局层），决定配置和数据位置
2. `fuyao_app::start` 完成全部装配：加载配置 → 批量构造 Provider 实例 → 收集工具（内置 + MCP）→ 装配 LoopGuardPlugin → 启动引擎 → 包装成 `App`（fan-in 单一出口），返回 `FuyaoApp { app, sessions }`——`app` 跑对话，`sessions` 查历史
3. `app.create_session` 从零创建一个新对话，返回 session_id（同时构建系统提示词、落库元数据、spawn session task；该 session 的 rx 由 App 内部 forwarder 消费进 fan_out）
4. `app.send` 把用户消息经单一入口路由到对应 session 的入站通道（不阻塞）
5. `app.recv` 从单一出口取出事件流——`Chunk` 是流式文本片段，`Assistant` 是完整回复
6. `app.shutdown` 优雅关闭（引擎先 shutdown → 所有 session task 落库退出 → forwarder 退出 → MCP 服务器停机 → drop App flush 日志）

## 恭喜

你的 Agent 已具备：

- 流式对话能力（多 session 并发，互不阻塞）
- 内置工具（read / write / bash / grep / glob / edit / webfetch / skill / todowrite）
- 会话持久化（SQLite，事件级落库）
- 上下文压缩（token 逼近上限时自动摘要）
- 费用统计（每条 assistant 消息独立计费）
- 自动重命名（首条消息后异步生成标题）
- 循环检测防护（LoopGuardPlugin）
- LLM 重试（RateLimit / Timeout 等可恢复错误自动重试）

## 下一步

- [理解 ReAct 循环](../解释/引擎内核设计.md) — Agent 怎么思考和调用工具
- [加自定义工具](../指南/加自定义工具.md) — 扩展 Agent 能力
- [核心架构](../解释/核心架构.md) — 整体设计全貌（两层分离 / 单一入口出口 / 多 session 并发）
