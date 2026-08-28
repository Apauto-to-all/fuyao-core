# 创建 Agent 定义

## 先分清两个「Agent」概念

| 概念 | 本质 | 位置 | 决定 |
| --- | --- | --- | --- |
| agent_id | 独立 Agent 实体（数据隔离单元） | `fuyao-agents/{id}/`（一个目录） | 数据在哪：sessions.db、缓存、agent 层配置 |
| Agent 定义 | 提示词能力（终端用户称「智能体」） | `agents/{名}.md` | 内容是什么：系统提示词、行为指令 |

两者正交：任何 agent_id 都可以在创建会话时指定任何具名定义（如 agent_id=`coder` 的会话用 `reviewer` 定义）。会话使用的定义由创建参数显式指定——引擎层必填、不提供隐式兜底，应用层惯例缺省传 `default`。

## 目录与优先级

| 层 | 路径 | 适用 |
| --- | --- | --- |
| workspace | `{工作目录}/.fuyao/agents/{名}.md` | 项目专属定义 |
| agent | `{agent_root}/agents/{名}.md` | 独立 Agent 私有定义（可携带私有 `default` 覆盖同名共享定义） |
| global | `~/.fuyao/agents/{名}.md` | 个人通用定义 |
| extra | `{插件根}/agents/{名}.md` | 插件分发 |
| 内置 | `default` / `explore` / `executor` | 兜底，恒为最低优先级 |

- 文件 stem（去掉 `.md` 的文件名）即定义名，按层序首现胜出
- 文件不存在 → 落到下一层；文件存在但 frontmatter 损坏 → **直接报错**，不静默跳层

## frontmatter 字段

| 字段 | 必填 | 取值 / 类型 | 缺省 |
| --- | --- | --- | --- |
| `name` | 否 | string | 空串 |
| `description` | 否 | string | 空串 |
| `version` | 否 | string | 空串 |
| `author` | 否 | string | 空串 |
| `mode` | 否 | `primary` / `subagent`（大小写不敏感，未知值报错） | `primary` |
| `tools` | 否 | table：工具名 → bool | 空（全部启用） |

- `tools` 中**未列出的工具默认启用**，显式 `false` 才禁用；与全局 `[tools.enabled]` 取交集生效
- 没有 `model` 字段——模型选择走 fuyao.toml（`[models]` / `[providers]`）与会话参数，不进定义
- frontmatter 之后的全部正文即**系统提示词**
- YAML 顶层必须是 mapping，语法错误直接报错（fail-loud，与技能的宽松解析不同）

## mode 语义

- `primary`：主 Agent 定义，创建会话时使用
- `subagent`：子代理定义，经 `subagent` 工具（`subagent_type` 参数）调用——跑完整 ReAct 循环，终态回复作为工具返回值；子代理内不可再见 subagent 工具（防递归）
- 内置 `explore` 是范例：`mode: subagent` + `tools` 收窄（`write: false, edit: false`）做只读探索

## 创建步骤

1. **选层选名**：共享定义放 global 或 workspace；独立 Agent 私有放其 `{agent_root}/agents/`
2. **写文件** `agents/{名}.md`：frontmatter（至少 `name`、`description`、`mode`）+ 正文系统提示词
3. **收窄权限**：只读型子代理在 `tools` 把 `write` / `edit` / `bash` 等置 `false`
4. **验证**：primary 定义在创建会话时指定使用；subagent 定义经 `subagent` 工具调用（类型名不存在时返回可用列表）
