# 5 分钟跑起第一个 Agent

> 本教程带你从零创建一个能对话的 Agent。完成后你将理解配置、启动、消息收发的完整流程。

## 前置条件

- Rust 工具链（cargo）
- 一个 OpenAI 兼容的 API Key（如 DeepSeek）

## 第 1 步：配置 fuyao.toml

在项目根创建 `fuyao.toml`：

```toml
model = "deepseek/deepseek-v4-flash"

[providers.deepseek]
name = "DeepSeek"
api_key_env_vars = ["DEEPSEEK_API_KEY"]

[providers.deepseek.options]
base_url = "https://api.deepseek.com/v1"

[providers.deepseek.models."deepseek-v4-flash"]
name = "deepseek-v4-flash"
```

**预期结果**：配置文件创建成功，指定了模型和供应商。

## 第 2 步：配置 .env

在项目根创建 `.env`：

```text
DEEPSEEK_API_KEY=sk-你的密钥
```

**预期结果**：环境变量文件创建成功。

## 第 3 步：启动 Agent

在你的项目中添加依赖并启动：

```rust
use fuyao_api::{AgentContext, OutputEvent};
use fuyao_app;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 构造 Agent 上下文（默认配置）
    let agent_ctx = AgentContext::default();

    // 一行启动：初始化引擎 + 注册工具 + MCP + 插件 + 日志
    let (engine, handle, app_ctx) = fuyao_app::start(agent_ctx).await?;

    println!("Agent 启动成功！");

    // 发送一条消息
    handle.send_message("你好，介绍一下你自己".to_string()).await;

    // 接收流式响应
    while let Some(event) = handle.next_event().await {
        match event {
            OutputEvent::Chunk(c) => {
                if let Some(text) = c.content {
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

    handle.shutdown().await;
    Ok(())
}
```

**预期结果**：终端输出 `Agent 启动成功！`，然后流式打印 AI 的回复，最后显示 `--- 回复完成 ---`。

## 发生了什么？

1. `AgentContext::default()` 创建默认上下文（全局层路径、默认模型）
2. `fuyao_app::start` 完成全部装配：加载配置 → 注册 Provider → 初始化引擎 → 注册工具 + 插件 + 日志
3. `handle.send_message` 发送用户消息到引擎输入通道
4. `handle.next_event` 接收引擎的输出事件流——`Chunk` 是流式文本片段，`Assistant` 是完整回复

## 恭喜

你的 Agent已具备：
- 流式对话能力
- 内置工具（read / write / bash / grep 等）
- 会话持久化（SQLite）
- 循环检测防护

## 下一步

- [理解 ReAct 循环](../解释/引擎内核设计.md) — Agent 怎么思考和调用工具
- [加自定义工具](../指南/加自定义工具.md) — 扩展 Agent 能力
- [核心架构](../解释/核心架构.md) — 整体设计全貌
