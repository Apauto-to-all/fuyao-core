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
```

> 主对话模型不在 fuyao.toml 配置——创建会话时在 `ModelConfig.model_id` 显式提供（如 `"deepseek/deepseek-v4-flash"`），`String` 必填非空，空值引擎拒绝对话。

在 .env 中设 API Key：

```text
DEEPSEEK_API_KEY=sk-xxxxxxxx
```

### 多 Provider 共存

所有 `[providers.*]` 段都会被注册进 `ProviderRegistry`，按 session 的 `SessionParams.model_config.model_id` 路由：

```toml
[providers.deepseek]
name = "DeepSeek"
api_key_env_vars = ["DEEPSEEK_API_KEY"]
# ...

[providers.aliyun]
name = "Aliyun"
api_key_env_vars = ["DASHSCOPE_API_KEY"]

[providers.aliyun.options]
base_url = "https://dashscope.aliyuncs.com/compatible-mode/v1"

[providers.aliyun.models."qwen3.6-plus"]
name = "qwen3.6-plus"

# 轻量任务模型（标题生成、压缩摘要等用）
[models.fast]
model = "aliyun/qwen3.6-plus"
```

```rust
// 创建会话时必须显式提供 model_id（String 必填），引擎不提供隐式兜底，空值拒绝对话
let id = app.create_session(SessionParams {
    model_config: ModelConfig {
        model_id: "deepseek/deepseek-v4-flash".to_string(),
        ..Default::default()
    },
    ..Default::default()
}).await?;

// 显式指定 aliyun 的模型（创建时在 model_config 定）
let id = app.create_session(SessionParams {
    model_config: ModelConfig {
        model_id: "aliyun/qwen3.6-plus".to_string(),
        ..Default::default()
    },
    ..Default::default()
}).await?;

// 运行时切模型：调 Engine::update_session_params(&id, new_params)，下一轮生效（签名见 rustdoc）
```

## 方式二：实现 Provider trait

非 OpenAI 兼容的供应商，实现 Provider trait：

```rust
use fuyao_provider::{Provider, StreamEvent, StreamError, BoxStream, ChatRequest, ChatResponse, StreamOptions};

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
        // 非流式调用（标题生成用）
    }
}
```

然后用 `ProviderRegistry::with_instance` 装配进引擎：

```rust
use fuyao_provider::ProviderRegistry;
use std::sync::Arc;

let my_provider = Arc::new(MyProvider::new(/* ... */));
let providers = ProviderRegistry::with_instance("my_provider", my_provider);

let engine = Engine::new(
    EngineParams { agent_paths },
    providers,
    tools,
    plugin_host,
).await;
```

## 验证

```rust
let app = fuyao_app::start(EngineParams { agent_paths }).await?;

// 创建会话时显式提供 model_id（String 必填），引擎不提供隐式兜底
let session_id = app.create_session(SessionParams {
    model_config: ModelConfig {
        model_id: "deepseek/deepseek-v4-flash".to_string(),
        ..Default::default()
    },
    ..Default::default()
}).await?;
app.send(&session_id, InputEvent::User(msg)).await?;

// 观察 OutputEvent::Chunk 流式输出
while let Some(event) = app.recv().await {
    if let OutputEvent::Assistant(_) = event { break; }
}
```

## 错误处理

- **Provider 创建失败**（API Key 未配等）：`init_engine` 仅 WARN 跳过该 Provider，其他继续注册；若所有 Provider 都失败返 `InitError::NoProviderAvailable`
- **错误的 model_id**（provider_id 未注册）：`resolve_model` 在 turn.rs build 阶段 fail-loud，发 `OutputEvent::Error`（错误信息含可用 provider 列表）
- **错误的 model 名**（provider 存在但 model 不存在）：走 HTTP 404 链路，`is_retryable` 判 false 冒泡为 `OutputEvent::Error`
- **413 上下文溢出**：归类为 `StreamError::ContextOverflow`，引擎不自动压缩，按「fail loud + 上层决策」原则报 Error 让上层处理

## 相关

- [Provider 设计](../解释/Provider设计.md) — trait 抽象、SSE 解码、ProviderRegistry 多 Provider 路由
- [配置项参考](../参考/配置项参考.md) — `[providers.*]` / `[models.*]` 全字段
- [错误类型参考](../参考/错误类型参考.md) — StreamError / ProviderError 变体
