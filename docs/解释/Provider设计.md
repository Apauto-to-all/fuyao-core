# Provider 设计

> 本文解释 LLM 供应商抽象、OpenAI 兼容实现、多 Provider 路由、流式解码与注册表。API 签名见 `cargo doc --workspace`。

## Provider trait

统一所有 LLM 供应商的核心抽象：

```text
trait Provider {
    fn stream_chat(request, model, options) -> BoxStream<Result<StreamEvent, StreamError>>
    fn chat(request, model, options) -> Result<ChatResponse, StreamError>
}
```

- `stream_chat`：流式调用，返回 BoxStream（核心方法，ReAct 循环用）
- `chat`：非流式调用（响应一次性返回）；与 stream_chat 共享同一套 options（含思考配置），思考能力等价支持

### StreamEvent

```text
StreamEvent = TextDelta { content }                        // 文本增量
            | ReasoningDelta { content }                   // 推理增量
            | ToolCallChunk { index, id?, name?, args_delta? }  // 工具调用片段
            | Done { usage: StreamUsage, finish_reason }   // 流结束 + 用量 + 完成原因
```

FinishReason = `Stop` | `ToolCalls` | `Length`

## OpenAI 兼容实现

`OpenAIProvider` 基于 reqwest 自建 HTTP 客户端（非 async-openai 封装）。**provider-agnostic**——只持 api_key + base_url，不存 provider_id，可同时作为多个供应商的实例。

### 为什么自建 HTTP？

流式 SSE 解码 + 重试退避 + 多供应商注册需要细粒度控制。封装库假设单一供应商，而 fuyao 需要多供应商注册 + 按 agent 路径缓存。

### SSE 流式解码（两层）

流式解码分两层，各自独立：

**第一层：SSE 字节流解析**（内联于 `stream_chat`）

- UTF-8 跨 chunk 缓冲：流可能从多字节字符中间切断，用 `utf8_buf` 累积，`std::str::from_utf8` 失败时取 `valid_up_to`，不足 4 字节则等更多数据
- SSE 行缓冲：按 `\n` 切割，剥离 `data:` 前缀，`[DONE]` 终止
- reasoning 字段兼容：`reasoning_content`（DeepSeek/Qwen）与 `reasoning`（部分供应商）双字段名
- `include_usage` 最终 chunk：`choices` 为空但顶层有 usage 时正确提取

**第二层：StreamDecoder 状态机**（消费 `StreamEvent`，输出 `OutputEvent`）

```text
TextDelta / ReasoningDelta → 转为 Chunk 事件输出
ToolCallChunk → 按 index 累积拼接（id / name / args_delta），返回空（等流结束）
Done → 仅更新 usage，返回空
```

- 流结束后 `take_tool_calls()` 取出完整工具调用（过滤 id/name 非空），**按 id 字符串排序**（注意：按 id 而非 index，多工具调用时可能与 LLM 输出顺序不一致）
- `peek_tool_calls()`：不消费地读取（中断时取部分结果）

### HTTP 错误分类

```text
HTTP 响应非 2xx 时 classify_http_error：
  401 / 403        → AuthError（认证失败，不可重试）
  429              → RateLimit（含 Retry-After 头解析，可重试）
  413              → ContextOverflow（上下文溢出，不可重试）
  其余 → 检查 body 内容：
    含 "context_length_exceeded" / "maximum context length"
      → ContextOverflow（上下文溢出，不可重试）
    否则 → ApiError("HTTP {code}: {body}")
```

- 5xx 的 `ApiError` 由 `is_retryable` 判定为可重试（通过消息子串匹配 "500"/"502" 等）
- 未知 `finish_reason` 字符串一律回落 `Stop`
- 413 归类为 `ContextOverflow` 供上层识别——引擎不兜底自动压缩（详见 [会话系统设计](会话系统设计.md) 的"不实施的项"）

### 思考字段注入（thinking_type / reasoning_effort）

`StreamOptions` 携带 `thinking_type` 和 `reasoning_effort`，在构建请求体时各自独立注入：

| 字段 | Some 时行为 | None 时行为 |
|------|------------|------------|
| `thinking_type` | 请求体加 `thinking: { type: "enabled" / "disabled" }` | 不发 `thinking` 字段 |
| `reasoning_effort` | 请求体加该档位名字符串（透传） | 不发该字段 |

**两字段正交独立**：各自为 Some 时各自发送，互不压制。fuyao 不替服务器做语义裁剪——不因 `thinking_type = Disabled` 而压掉 `reasoning_effort`，配了就必发，由服务器各自解释。

> `reasoning_effort` 的可选值由模型配置的 `reasoning_efforts` 字段声明（如 `["low","medium","high","max"]`），但实际发送时 fuyao 不校验档位名，透传原始字符串。

## 多 Provider 路由（ProviderRegistry）

引擎不持单个 `Arc<dyn Provider>` 实例，而是持 `ProviderRegistry`——按 provider_id 索引的实例集合。

```text
ProviderRegistry {
    instances: HashMap<String, Arc<dyn Provider>>,  // key 已小写规范化
}

impl ProviderRegistry {
    from_registered(agent_paths) -> Self        遍历 list_providers 逐个建实例
    get(provider_id) -> Option<Arc<dyn Provider>>  大小写不敏感取实例
    is_empty() -> bool
    provider_ids() -> Vec<String>
    with_instance(provider_id, instance) -> Self  测试 / 装配辅助
}
```

### 路由机制

session 的 `SessionParams.model_config.model_id`（`String` 必填非空）形如 `"provider_id/model_id"`——session task 在执行该轮 LLM 调用前：

1. `resolve_model(params)` 拆 model_id（provider_id 小写、model 原样）；空值 fail-loud 发 Error（引擎不提供隐式兜底，直接拒绝对话）
2. `ctx.providers.get(&resolved.provider_id)` 取 Provider 实例
3. 取不到 → fail-loud 发 `OutputEvent::Error`（错误信息精准："provider_id 'xxx' 未注册，可用: [...]"）
4. 把 Provider 实例传给 `run_stream_with_retry`

**retry / stream 模块完全不感知 registry**——保持职责单一（只关心一个 Provider 实例），多 Provider 透明。

### init 时的容错

- 单个 Provider 实例失败（API Key 没配）仅 WARN 跳过，其他继续注册——支持渐进配置
- 所有 Provider 都失败 → `Err(InitError::NoProviderAvailable)` 启动失败

## StreamError 的 Cancelled 变体

```text
StreamError::Cancelled  // 操作被取消（如引擎关闭）
```

- 表示"非错误的取消"（shutdown 触发），`#[error("操作被取消")]`
- `is_retryable(Cancelled) = false`（防止 retry 内部循环重试）
- retry 把它冒泡给 turn.rs 走中断路径（不发 Error 事件）

由 `RetryRunner` 在退避 sleep 期间收到 `shutdown_token` 信号时产生，让 shutdown 立即冒泡不依赖 sleep 自然到期。

## 重试与退避

```text
retry.rs:
  backoff_duration(attempt, headers) -> Duration
  is_retryable(status, error) -> bool
```

- **退避**：指数退避，起始 2000ms（可配）
- **响应头优先**：服务器返回 `Retry-After` 时尊重它
- **可重试判定**：429 / 5xx / Timeout / Connection 可重试，4xx / AuthError / StreamParseError / ContextOverflow / Cancelled 不重试
- **max_retries 默认 `u32::MAX`**（无限重试）——可恢复错误说明供应商侧短时不可用，引擎应持续重试直到恢复；嫌激进可在 TOML 里配小

**重试位置**：react 层 `fuyao-core/src/react/retry.rs::run_stream_with_retry`（不在 provider 层）——因为发 Retry 事件需要 session_id（详见 [引擎内核设计](引擎内核设计.md)）。

配置：`[llm.retry]`（见 [配置项参考](../参考/配置项参考.md)）。

## 注册表

```text
registry.rs:
  全局 provider + model 注册表（按 agent 路径缓存）
```

| API | 说明 |
|-----|------|
| `register_provider(provider_id, provider, cache_key)` | 注册供应商配置 |
| `register_model(model_id, model, cache_key)` | 注册模型 |
| `get_provider(agent_paths, provider_id)` | 获取供应商配置 |
| `get_model(agent_paths, model_id)` | 获取模型 |
| `list_providers(agent_paths)` | 列出供应商 |
| `list_models(agent_paths)` | 列出模型 |
| `clear_cache()` | 清空缓存（测试用） |
| `agent_paths_cache_key(agent_paths)` | 构造缓存 key |

**按 agent 路径缓存**：不同 agent_id 有不同的 provider/model 配置，缓存以 agent_paths 为 key 隔离。

### 手动解析（load_providers）

`[providers.*]` 段不走 serde 默认反序列化，而是单独手动解析：

- 条目必填报错：Provider / Model 条目不是 table、缺 `name`、`name` 非字符串，或模型缺 `limit.context` / 值非法，均判为配置错误（fail-loud），整个配置加载失败，错误信息带 `provider_id/model_id` 定位
- 价格字段（`cost.input` 等）兼容整数和浮点（`input = 2` 和 `input = 2.0` 等价）
- 解析成功后回填到 `FuyaoConfig.providers`（该字段 `#[serde(skip)]` 不参与 serde）

### 工厂

```text
client.rs:
  parse_model_id("deepseek/deepseek-v4-flash") → ("deepseek", "deepseek-v4-flash")
  create_provider(agent_paths, provider_id) → OpenAIProvider
```

## 装配流程（fuyao-app::init_engine）

```text
init_engine(agent_paths):
  1. load_env + load_config         ← 加载配置
  2. ensure_registered              ← 注册 Provider + Model（从 [providers] 配置）
  3. ProviderRegistry::from_registered ← 批量构造所有已注册 Provider 的实例
  4. is_empty 检查                   ← 全部失败返 NoProviderAvailable
  5. 返回 (ProviderRegistry, log_guard)
```

由 `fuyao-app::start` 串联：`init_engine` → `build_tool_registry` → `Engine::new(params, provider, tools, plugin_host)`。

## 关键设计决策

### 为什么不用 async-openai？

async-openai 假设单一 OpenAI 供应商，而 fuyao 需要：多供应商注册、按 agent 路径隔离配置、自定义 base_url（兼容 DeepSeek / 阿里云 / 本地模型等）。自建 HTTP + SSE 解码器只需几百行，换来完全的控制力。

### 为什么注册表按 agent 路径缓存？

不同 agent_id 有不同的 fuyao.toml（三层合并），provider/model 配置可能不同。按 agent_paths 缓存确保隔离——切换 agent 时不会拿到错误的 provider 配置。

### 为什么 stream_chat 是核心方法？

ReAct 循环需要流式输出——用户实时看到 AI 的思考过程，且工具调用可以在流完成后立即执行。非流式 chat 只用于标题生成（短文本不需要流式增量）。上下文压缩用 `stream_chat`（前端要实时看到摘要生成）。

### 为什么 retry 尊重 Retry-After 头？

服务器知道自己的负载状况。`Retry-After` 头是服务器建议的等待时间，通常比客户端的指数退避更准确。有头时用头的值，无头时用指数退避。

### 为什么重试在 react 层而不在 provider 层？

发 `OutputEvent::Retry` 需要 session_id 标签（事件全程标签原则）。provider 层的全局装饰器拿不到 session_id——曾尝试过 `RetryingProvider` 装饰器方案，因拿不到 session_id 发不了 per-session 事件而推翻。最终方案是 `fuyao-core/src/react/retry.rs::run_stream_with_retry`（react 层，持 `&SessionCtx` 天然有 session_id）。**架构选型必须从「事件流」倒推**，不能只看代码侵入度。

### 为什么 413 不自动压缩？

413 多为模型切换导致的用户使用错误（如 128K 模型跑到 100K 后切到 8K），引擎替用户兜底反而是"悄悄降级"——压缩吃掉原始上下文细节后仍可能 413，但信息已损失。按「引擎 fail loud + 上层应用决策」原则：413 直接报错让上层处理（提示用户、切回大模型、或允许用户主动压缩）。
