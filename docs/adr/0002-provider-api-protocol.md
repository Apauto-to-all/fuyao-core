# 供应商 API 协议：必填单字段 `api_protocol`、三枚举、构造期分派

引擎要支持多种 wire 协议（OpenAI Chat Completions / OpenAI Responses / Anthropic Messages），协议选择是供应商级属性。我们决定：`[providers.<id>]` 顶层增加必填字段 `api_protocol`，取值三枚举 `openai-completions` / `openai-responses` / `anthropic-messages`（kebab-case）；ProviderRegistry 构造实例时按它单点分派——当前仅 `openai-completions` 有 wire 实现，其余两值构造该供应商时报「尚未实现」WARN 并跳过（不拖垮其余供应商，全空才报 `NoProviderAvailable`）。admin 管理面 `ProviderSpec` 同步携带该字段。

## 备选方案（否决理由）

- **缺省 `openai`（省略即 OpenAI 兼容）**（否决）：开发期不做兼容处理；必填写法让每个供应商的协议在配置里显式可见，不留「不写 = 猜」的空间。
- **协议名本位取值 `chat-completions` / `responses` / `messages`**（否决）：`messages` 脱离家族前缀后无法辨识归属；家族前缀式三值齐整，未来第四家供应商扩展自然。
- **放进 `options` 子段（与 `base_url` / `api_key` 为伴）**（否决）：`options` 是连接凭据类选项（去哪、拿什么钥匙），协议方言是供应商的本质属性（说什么话），层级混放。
- **配了未实现格式即引擎启动硬报错**（否决）：与「单个供应商构造失败仅 WARN 跳过」的既有容错粒度不一致，一个配置超前于实现的供应商不应拖垮其余全部可用供应商。

## 后果

- `openai-responses` / `anthropic-messages` 当前是「配置可写、加载可过、构造期明确报未实现」的占位值——地基只立分派接缝，wire 实现后续按枚举逐个填充。
- admin 面 `ProviderSpec.api_protocol` 为必有字段（非 Option），create / update 载荷恒携带；不存在「清除回落缺省」语义——清除即配置非法。
- 既有全部 `fuyao.toml` 与测试夹具需一次性补 `api_protocol` 行，不留兼容读取路径。
