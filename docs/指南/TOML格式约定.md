# TOML 格式约定

fuyao.toml 用 TOML **内联表**（inline table，`{ key = value }`）精简配置，避免一堆零散的 `[xxx]` 子节。本文给出何时内联、何时保留子节的判断规则，以及常见场景的精简写法。

> 各字段的含义、类型与默认值见 [`配置项参考`](../参考/配置项参考.md)。本文只讲**写法格式**，不讲字段语义。

## 判断规则：内联还是拆子节

一个嵌套结构该用内联表 `{ }` 还是独立子节 `[ ]`，看三点：

| 条件 | 选择 |
| ---- | ---- |
| 字段少（1~3 个）、且是叶子数据 | **内联** |
| 有数组表（如阶梯计费 `tiers`） | **内联数组**（见下） |
| 字段多 / 层级深 / 需要逐字段加注释 | **拆子节** |

一句话总结：**能内联就内联，保持一个实体一个小节头**。

两条可读性经验：

- **每个模型保留独立节头** `[providers.xxx.models.<id>]`，不要把所有模型挤进同一个 `[providers.xxx.models]` 下内联
- **`[models]` 是例外**：`fast` 字段少（`model` 必填，`thinking_type` / `reasoning_effort` 可选），适合全部内联

## 常见场景的精简写法

下面每个场景给出「冗余写法（避免）」和「精简写法（推荐）」对比。

### 连接选项 options

单字段连接选项，直接内联：

```toml
# 避免：多开一个子节
[providers.deepseek.options]
base_url = "https://api.deepseek.com"

# 推荐：内联
[providers.deepseek]
name = "DeepSeek"
options = { base_url = "https://api.deepseek.com" }
```

### 模型属性 cost / limit / modalities

模型的 `cost`、`limit`、`modalities` 都是叶子表，内联。其中 `limit.context` **必填且为正整数**（缺失 / 为 0 / 类型不符会导致整个配置加载失败，引擎启动时报错并带 `provider_id/model_id` 定位），因此 `limit` 内联时至少写 `context`：

```toml
# 避免
[providers.deepseek.models."deepseek-v4-flash".cost]
input = 2
output = 12.0
[providers.deepseek.models."deepseek-v4-flash".limit]
context = 128000
output = 8192

# 推荐
[providers.deepseek.models."deepseek-v4-flash"]
name = "deepseek-v4-flash"
cost = { input = 2, output = 12.0 }
limit = { context = 128000, output = 8192 }
```

### 阶梯计费 tiers

TOML 支持内联表内嵌数组，因此 `[[cost.tiers]]` 数组表可以内联成 `cost = { tiers = [ ... ] }`：

```toml
# 避免
[[providers.aliyun.models."qwen-max".cost.tiers]]
max_tokens = 256000
input = 1.2
output = 7.2
[[providers.aliyun.models."qwen-max".cost.tiers]]
max_tokens = 1000000
input = 4.8
output = 28.8

# 推荐：tiers 内联为数组，逐项换行保持可读
[providers.aliyun.models."qwen-max"]
cost = { tiers = [
    { max_tokens = 256000, input = 1.2, output = 7.2 },
    { max_tokens = 1000000, input = 4.8, output = 28.8 },
] }
limit = { context = 1000000, output = 65536 }
```

### 多模型标签 models

`[models]` 下每个标签字段少（`model` 必填，思考两字段可选），全部内联：

```toml
# 避免
[models.fast]
model = "deepseek/sensenova-6.7-flash-lite"

# 推荐：思考字段同样内联进表，可选不填即走模型默认
[models]
fast = { model = "deepseek/sensenova-6.7-flash-lite", thinking_type = "enabled", reasoning_effort = "high" }
```

### 工具开关 tools

单个工具开关内联成一行：

```toml
# 避免
[mcp_servers.exa.tools]
web_fetch_exa = false

# 推荐
[mcp_servers.exa]
url = "https://mcp.exa.ai/mcp"
tools = { web_fetch_exa = false }  # 禁用：与内置 webfetch 功能重叠
```

## 行尾注释

内联表的字段值可照常加行尾注释，与独立字段等价：

```toml
cost = { input = 2, output = 12.0 }     # input=2，output=12
limit = { context = 128000, output = 8192 }  # 上下文 128K，输出 8K
```

## 精简写法示例

把上面规则合到一起，一个典型的精简配置片段：

```toml
# 多模型标签：内联（思考字段可选，配了就内联进表）
[models]
fast = { model = "deepseek/sensenova-6.7-flash-lite" }

# 供应商 + 连接选项：options 内联
[providers.deepseek]
name = "DeepSeek"
api_key_env_vars = ["DEEPSEEK_API_KEY"]
options = { base_url = "https://api.deepseek.com" }

# 每个模型一个节头，cost / limit 内联
[providers.deepseek.models."deepseek-v4-flash"]
name = "deepseek-v4-flash"
cost = { input = 2, output = 12.0 }
limit = { context = 128000, output = 8192 }

# 阶梯计费：tiers 内联为数组
[providers.aliyun.models."qwen-max"]
cost = { tiers = [
    { max_tokens = 256000, input = 1.2, output = 7.2 },
    { max_tokens = 1000000, input = 4.8, output = 28.8 },
] }
limit = { context = 1000000, output = 65536 }
modalities = { input = ["text", "image"], output = ["text"] }

# 工具开关：内联
[mcp_servers.exa]
url = "https://mcp.exa.ai/mcp"
tools = { web_fetch_exa = false }  # 禁用：与内置 webfetch 功能重叠
```

> 完整字段配置示例见 [`配置项参考`](../参考/配置项参考.md) 的「完整示例」一节。
