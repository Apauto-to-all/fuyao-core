# 接入新 LLM 供应商

> OpenAI 兼容接口直接配 fuyao.toml；自定义协议实现 Provider trait。

## 方式一：OpenAI 兼容（推荐）

大多数供应商（DeepSeek / 阿里云 / Moonshot / 本地 Ollama）兼容 OpenAI 接口，只需配 fuyao.toml：

```toml
[providers.deepseek]
name = "DeepSeek"
api_key_env_vars = ["DEEPSEEK_API_KEY"]

[providers.deepseek.options]
base_url = "https://api.deepseek.com/v1"

[providers.deepseek.models."deepseek-v4-flash"]
name = "deepseek-v4-flash"

[providers.deepseek.models."deepseek-v4-flash".limit]
context = 128000
output = 8192

model = "deepseek/deepseek-v4-flash"
```

在 .env 中设 API Key：

```text
DEEPSEEK_API_KEY=sk-xxxxxxxx
```

## 方式二：实现 Provider trait

非 OpenAI 兼容的供应商，实现 Provider trait：

```rust
use fuyao_provider::{Provider, StreamEvent, StreamError, BoxStream};
use async_trait::async_trait;

#[async_trait]
impl Provider for MyProvider {
    fn stream_chat(
        &self,
        request: ChatRequest,
        model: &str,
        options: StreamOptions,
    ) -> BoxStream<'_, Result<StreamEvent, StreamError>> {
        // 实现 HTTP 调用 + SSE 解码 + StreamEvent 产出
    }

    async fn chat(
        &self,
        request: ChatRequest,
        model: &str,
    ) -> Result<ChatResponse, StreamError> {
        // 非流式调用（上下文压缩用）
    }
}
```

然后注册到注册表：

```rust
register_provider(&agent_paths, MyProvider::new(...));
register_model(&agent_paths, model);
```

## 验证

```rust
let (engine, handle, _) = fuyao_app::start(agent_ctx).await?;
handle.send_message("你好".to_string()).await;
// 观察 OutputEvent::Chunk 流式输出
```

## 相关

- [Provider 设计](../解释/Provider设计.md) — trait 抽象、SSE 解码、注册表
- [配置项参考](../参考/配置项参考.md) — `[providers.*]` 全字段
