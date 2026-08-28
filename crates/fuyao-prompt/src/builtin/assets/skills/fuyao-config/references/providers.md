# 添加供应商

## 单一事实源原则

供应商定义**只允许写在全局层** `~/.fuyao/fuyao.toml` 的 `[providers]` 段：

- agent / workspace 层的 fuyao.toml 出现 `[providers]` 键 → 整个配置加载失败（启动即报错），不存在「被高优先级层盖掉」的模糊状态
- 管理面（供应商增删改查）与加载面读写同一份落盘，永远是同一个事实源
- 分层的是「选择」（模型引用如 `[models.fast]` 走三层合并），单点的是「定义」

## 完整定义示例

```toml
[providers.deepseek]
name = "DeepSeek 深度求索"                  # 显示名（必填）
api_protocol = "openai-completions"        # 协议（必填）：openai-completions / anthropic-messages
api_key_env_vars = ["DEEPSEEK_API_KEY"]    # API Key 环境变量名（推荐）

[providers.deepseek.options]
base_url = "https://api.deepseek.com"      # 自定义 base URL

[providers.deepseek.models."deepseek-v4-flash"]     # 模型 ID = TOML 键名
name = "DeepSeek v4 Flash"                 # 模型显示名（必填）
limit = { context = 1000000, output = 384000 }      # context 必填
cost = { input = 1.5, output = 4.5, cache = 0.05 }  # 价格/M tokens
reasoning_efforts = ["low", "high", "max"]          # 思考强度档位，可选
```

梯度价格模型（阿里云百炼 qwen3.6-flash）。`cost.tiers` 非空时整体取代非梯度价格，按**单次请求的 prompt_tokens** 命中分档（≤ `max_tokens` 即该档，超出全部区间取末档兜底），分档上限应落在 `limit.context` 以内：

```toml
[providers.aliyun]
name = "阿里云百炼"
api_protocol = "openai-completions"
api_key_env_vars = ["DASHSCOPE_API_KEY"]

[providers.aliyun.options]
base_url = "https://dashscope.aliyuncs.com/compatible-mode/v1"

[providers.aliyun.models."qwen3.6-flash"]
name = "Qwen3.6 Flash"
limit = { context = 1000000, output = 65536 }
modalities = { input = ["text", "image"], output = ["text"] }   # 可选，默认纯文本

[providers.aliyun.models."qwen3.6-flash".cost]

[[providers.aliyun.models."qwen3.6-flash".cost.tiers]]
max_tokens = 256000        # prompt_tokens ≤ 256K 走这档
input = 1.2
output = 7.2
cache = 0.24

[[providers.aliyun.models."qwen3.6-flash".cost.tiers]]
max_tokens = 1000000       # 256K 以上走这档（末档兜底）
input = 4.8
output = 28.8
cache = 0.96
```

引用模型时用 `provider_id/model_id` 格式，如 `deepseek/deepseek-v4-flash`。需要梯度价格（`cost.tiers`）时，`cost` 必须写成段表形式——内联表 `{ … }` 定义后不能再被 tiers 扩展，混用会导致 TOML 解析失败。

## 字段说明

Provider 级：

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `name` | 是 | 显示名 |
| `api_protocol` | 是 | `openai-completions`（OpenAI 兼容）或 `anthropic-messages` |
| `api_key_env_vars` | 否 | API Key 环境变量名列表，按序查找第一个有值的 |
| `options.base_url` | 否 | 自定义接口地址 |
| `options.api_key` | 否 | 明文 API Key，解析优先级**高于**环境变量 |

Model 级（`[providers.{id}.models.{模型ID}]`）：

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `name` | 是 | 显示名 |
| `limit.context` | 是 | 上下文窗口（正整数）；上下文压缩的触发公式直接消费该值，缺失即加载报错 |
| `limit.input` / `limit.output` | 否 | 单次输入 / 输出上限；`input` 不应超过 `limit.context`（输入必须装得进上下文窗口） |
| `cost.input` / `cost.output` / `cost.reasoning` / `cost.cache` | 否 | 单价，整数或浮点均可 |
| `modalities.input` / `modalities.output` | 否 | 支持的模态（如 `["text", "image"]`），默认纯文本 |
| `reasoning_efforts` | 否 | 思考强度档位名列表，任意字符串透传 |
| `cost.tiers` | 否 | 梯度价格：多组 `{max_tokens, input, output, reasoning, cache}`（各单价可选）；按**单次请求的 prompt_tokens** 命中分档（≤ `max_tokens` 即该档，超出全部区间取末档），非空时整体取代非梯度价格四字段，分档上限应落在 `limit.context` 以内 |

## API Key 安全

- 解析优先级：`options.api_key`（明文）**先于** `api_key_env_vars` 环境变量——两者同配时生效的是明文
- 推荐只用 `api_key_env_vars`，密钥不进 fuyao.toml；变量值写在 `.env` 文件（三层路径同 fuyao.toml：`~/.fuyao/.env`、`{agent_root}/.env`、`{工作目录}/.fuyao/.env`，逐层覆盖），格式为 dotenv 语法：`KEY=value`，支持引号与 `#` 注释
- 多供应商 Key 各用独立变量名，避免串用

## 校验与报错（fail-loud）

以下问题都会让配置**整体加载失败**（不是跳过该条目）：

- `[providers]` 出现在 agent / workspace 层
- `[providers]` 段本身或某个条目不是 table
- 条目缺 `name`，或 `name` 不是字符串
- `api_protocol` 缺失 / 不是字符串 / 未知取值（报错会列出全部合法值）
- 模型缺 `limit.context`，或值 ≤ 0 / 类型错误（报错带 `{provider_id}/{model_id}` 定位）

## 生效方式

- 写盘不等于立即生效
- 应用层有热刷新入口（`reload_providers`）：重新三层加载 → 补载缺失环境变量 → 同名覆盖注册 → 清理落盘已删除的供应商；存活引擎下一 turn 生效，运行中的 turn 持旧实例跑完
- 无热刷新入口时，重启引擎生效
