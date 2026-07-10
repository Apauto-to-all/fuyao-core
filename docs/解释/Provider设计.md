# Provider 设计

> 本文解释 LLM 供应商抽象、OpenAI 兼容实现、流式解码与注册表。API 签名见 `cargo doc --workspace`。

## Provider trait

统一所有 LLM 供应商的核心抽象：

```text
trait Provider {
    fn stream_chat(request, model, options) -> BoxStream<Result<StreamEvent, StreamError>>
    fn chat(request, model) -> Result<ChatResponse, StreamError>
}
```

- `stream_chat`：流式调用，返回 BoxStream（核心方法，ReAct 循环用）
- `chat`：非流式调用（上下文压缩用）

### StreamEvent

```text
StreamEvent = TextDelta { content }                        // 文本增量
            | ReasoningDelta { content }                   // 推理增量
            | ToolCallChunk { index, id?, name?, args_delta? }  // 工具调用片段
            | Done { usage: StreamUsage, finish_reason }   // 流结束 + 用量 + 完成原因
```

FinishReason = `Stop` | `ToolCalls` | `Length`

## OpenAI 兼容实现

`OpenAIProvider` 基于 reqwest 自建 HTTP 客户端（非 async-openai 封装）。

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
  其余 → 检查 body 内容：
    含 "context_length_exceeded" / "maximum context length"
      → ContextOverflow（上下文溢出，不可重试）
    否则 → ApiError("HTTP {code}: {body}")
```

- 5xx 的 `ApiError` 由 `is_retryable` 判定为可重试（通过消息子串匹配 "500"/"502" 等）
- 未知 `finish_reason` 字符串一律回落 `Stop`

### 思考字段注入（thinking_type / reasoning_effort）

`AgentContext.model_config` 携带 `thinking_type` 和 `reasoning_effort`，在构建请求体时条件注入：

| 条件 | 行为 |
|------|------|
| `thinking_type = Some(Enabled)` | 请求体加 `thinking: { type: "enabled" }` |
| `thinking_type = Some(Disabled)` | **强制不发** `reasoning_effort`（强度对 Disabled 无意义） |
| `thinking_type = None` | 不发 `thinking` 字段；`reasoning_effort` 如有则发 |
| `reasoning_effort` | 接受**任意字符串**透传（fuyao 不校验档位名，由服务器决定） |

> `reasoning_effort` 的可选值由模型配置的 `reasoning_efforts` 字段声明（如 `["low","medium","high","max"]`），但实际发送时 fuyao 不校验。

## 重试与退避

```text
retry.rs:
  backoff_duration(attempt, headers) -> Duration
  is_retryable(status, error) -> bool
```

- **退避**：指数退避，起始 2000ms（可配）
- **响应头优先**：服务器返回 `Retry-After` 时尊重它
- **可重试判定**：429 / 5xx 可重试，4xx 不重试

配置：`[llm.retry]`（见 [配置项参考](../参考/配置项参考.md)）。

## 注册表

```text
registry.rs:
  全局 provider + model 注册表（按 agent 路径缓存）
```

| API | 说明 |
|-----|------|
| `register_provider(agent_paths, provider)` | 注册供应商 |
| `register_model(agent_paths, model)` | 注册模型 |
| `get_provider(agent_paths, provider_id)` | 获取供应商 |
| `get_model(agent_paths, model_id)` | 获取模型 |
| `list_providers(agent_paths)` | 列出供应商 |
| `list_models(agent_paths, provider_id)` | 列出模型 |

**按 agent 路径缓存**：不同 agent_id 有不同的 provider/model 配置，缓存以 agent_paths 为 key 隔离。

### 容错解析（load_providers）

`[providers.*]` 段不走 serde 默认反序列化，而是手动容错解析：

- 缺 `name` 的 Provider / Model 静默跳过（不报错）
- 价格字段（`cost.input` 等）兼容整数和浮点（`input = 2` 和 `input = 2.0` 等价）
- 解析成功后回填到 `FuyaoConfig.providers`（该字段 `#[serde(skip)]` 不参与 serde）

### 工厂

```text
client.rs:
  parse_model_id("deepseek/deepseek-v4-flash") → ("deepseek", "deepseek-v4-flash")
  create_provider(agent_paths, model_id) → OpenAIProvider
  create_provider_with_model(agent_paths, model_id) → (Provider, Model)
```

## 装配流程（fuyao-app::init_engine）

```text
init_engine:
  1. load_env + load_config         ← 加载配置
  2. ensure_registered              ← 注册 Provider + Model（从 [providers] 配置）
  3. 确定 model_id                   ← 显式 > 配置 > 首个已注册
  4. OpenAIProvider::new             ← 创建 Provider 实例
  5. get_model 校验                   ← 确认模型存在
  6. Engine::new(provider, model)    ← 装配引擎
```

## 关键设计决策

### 为什么不用 async-openai？

async-openai 假设单一 OpenAI 供应商，而 fuyao 需要：多供应商注册、按 agent 路径隔离配置、自定义 base_url（兼容 DeepSeek / 阿里云 / 本地模型等）。自建 HTTP + SSE 解码器只需几百行，换来完全的控制力。

### 为什么注册表按 agent 路径缓存？

不同 agent_id 有不同的 fuyao.toml（三层合并），provider/model 配置可能不同。按 agent_paths 缓存确保隔离——切换 agent 时不会拿到错误的 provider 配置。

### 为什么 stream_chat 是核心方法？

ReAct 循环需要流式输出——用户实时看到 AI 的思考过程，且工具调用可以在流完成后立即执行。非流式 chat 只用于上下文压缩（批量处理不需要流式）。

### 为什么 retry 尊重 Retry-After 头？

服务器知道自己的负载状况。`Retry-After` 头是服务器建议的等待时间，通常比客户端的指数退避更准确。有头时用头的值，无头时用指数退避。
