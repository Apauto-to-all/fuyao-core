# AnthropicMessages wire 实现：转换不变量、流式防护与类型最小扩容

`anthropic-messages` 协议从「配置可写、构造期报未实现」的占位值（ADR-0002）落地为可用 wire 实现。我们决定：在 `fuyao-provider` 新建 `anthropic/` 模块（照 `openai/` 模板：mod 薄 HTTP adapter + request 请求编码 + sse 线解码 + classify 错误分类 + completion 非流式模型，纯函数各自可单测），`factory.rs` 补分派一支。能力面对齐 OpenAI 侧：流式 + 非流式 `chat()`（标题生成）+ 工具循环 + images 多模态。

## 协议正确性硬边界（400 边界，配不变量测试）

Anthropic 对消息序列形状的校验比 OpenAI 严格，以下三条是适配器必须内建的不变量，且以专门测试锁定（产出序列永无相邻同角色消息、每个 tool_use 必有配对 tool_result）：

- **孤儿 tool_use 补 stub tool_result**：session 恢复、压缩边界、中断轮次后的历史重放中，assistant 的 tool_use 若无配对 tool_result，服务器直接 400。适配器扫描并合成 stub 结果兜底。
- **相邻同角色合并**：tool 结果在 Anthropic 协议里归 user 角色（`tool_result` content block），OpenAI 风格的「连续 tool 消息」「tool 后紧跟 user 消息」都会产出相邻同角色消息，必须合并进同一条。
- **`max_tokens` 必填**：Anthropic 无默认值。固定发 16384（claude 系输出上限普遍 ≥32k，保守值避免截断长工具调用序列）。

## 关键机制决策

- **usage 三桶归一化**：Anthropic 的 `input_tokens` 只是最后一个缓存断点之后的 token，总 prompt = `cache_read_input_tokens + cache_creation_input_tokens + input_tokens`（三桶互斥）。适配器求和归一到 `StreamUsage.prompt_tokens`，`prompt_cached_tokens` 取 `cache_read`，新增 `prompt_cache_creation_tokens: Option<u32>` 字段留痕（数据先留、费用计算暂不消费）。流式下 usage 分两处到达（message_start 带输入三桶、message_delta 带输出累计值），message_stop 时归一发出 Done 事件。
- **stop_reason 宽松映射**：`end_turn`→Stop、`tool_use`→ToolCalls、`max_tokens`→Length、其余（`pause_turn`/`refusal`/`stop_sequence`/未知）一律→Stop。FinishReason 枚举不扩容。
- **prompt caching**：恒挂两个 ephemeral 断点——system 提示尾 + 工具定义表尾（无条件的两个，实现最简收益最大）；对话尾断点不做。
- **SSE idle timeout**：reqwest 整体 `.timeout()` 对逐块消费的流式 body 不可靠，代理掐断连接后 `next().await` 永久挂起。anthropic 侧对流读取包 90s 逐行超时，映射 `StreamError::Timeout`（已可重试）。
- **serde 策略**：出方向类型化 enum（`#[serde(tag = "type")]`，序列化干净）+ 入方向宽松单结构体（全 Option 字段 + 按 kind 字符串分发，反序列化容错）。
- **认证**：只做 `x-api-key` + `anthropic-version: 2023-06-01`。不做 OAuth/setup-token 体系。
- **thinking 不支持**：遇 `thinking_type=Enabled` 打 WARN 日志并忽略（不发 thinking 字段，模型走默认应答）。明确告知不支持优于悄悄发错格式被 400。
- **LineAssembler 上提**：SSE 字节流行组装器（跨 chunk 半行缓冲）从 `openai/sse.rs` 上提到 crate 根成为协议共享底座；`[DONE]` 标记与 `event:` 行的语义解析仍归各协议模块。

## 备选方案（否决理由）

- **完整 extended thinking 支持**（否决）：Anthropic 开思考必须 `budget_tokens` 数字（`ThinkingType` 两值枚举无落点），且工具循环中 assistant 回传必须带服务器签名的 thinking 块——`ChatMessage.reasoning` 无 signature 字段，支持需横跨 fuyao-api / fuyao-provider / fuyao-session 四处改动（含持久化与压缩投影）。项目面向国内场景，Claude 深度思考需求低；第二版作为独立特性立项。
- **对话尾第三缓存断点**（否决）：需条件判断（非 system 消息数 > 1），边际收益低于实现与测试成本。
- **max_tokens 从 ModelLimit.output 推导**（否决）：请求构造路径需查模型配置，引入构造期依赖；固定常量保持路径零改动。
- **FinishReason 加 `pause_turn` / `refusal` 变体**（否决）：宽松映射已覆盖，扩容后无消费方。
- **ModelCost.cache 拆 cache_read / cache_write 两档**（否决）：牵动 fuyao-session 费用计算，且现有 cache 单价字段语义未定；与费用精化一起做。
- **zeroclaw 式字符串消息模型（content 编码 JSON 往返）**（否决）：类型安全丢失、每轮两次 JSON 反解；fuyao 已有类型化 ChatMessage，直接一次转换到位。
- **错误身份字符串化**（否决）：沿用现有 `StreamError` 结构化形态（status / RateLimit 带响应头），重试策略挂错误类型（`is_retryable` 已含 529，正是 Anthropic overloaded_error）。

## 后果

- `anthropic-messages` 成为可用协议；`openai-responses` 仍为占位值（下一个按此模式填充）。
- `StreamUsage` 新增 `prompt_cache_creation_tokens` 字段：旧会话数据天然无值（Option 兼容），费用计算暂不消费，为第二版费用精化留数据。
- OpenAI 侧 SSE 同样缺少 idle timeout（流假活隐患同在），记 TODO 本次不动——anthropic 侧验证期过后再推广。
- 设计输入与协议事实源：`.scratch/anthropic-messages/research-zeroclaw-anthropic.md`（zeroclaw `anthropic.rs` 源码研究，含行号引用）。面向场景以国内 Anthropic 兼容端点为主（如 GLM `https://open.bigmodel.cn/api/anthropic`），wire 格式严格对齐官方协议，默认 base_url 给官方地址、经配置指向兼容端点。
