# Agent 身份与路径设计

> 本文解释 agent_id 解析、三层路径系统、以及 agent_id 与 Agent 定义的正交关系。API 签名见 `cargo doc --workspace`。

## 两个正交的「Agent」概念

项目中存在两个完全正交、不可混为一谈的概念：

| 概念 | 本质 | 存储位置 | 决定什么 |
|------|------|---------|---------|
| **agent_id** | 独立 Agent 实体（数据隔离单元） | `fuyao-agents/{id}/`（一个目录） | 数据在哪：sessions.db、配置、日志 |
| **Agent 定义** | 提示词（一项能力 / 人格） | `agents/{name}.md`（一个文件） | 内容是什么：系统提示词、行为指令 |

**正交组合**：agent_id 为 `coder` 的独立 Agent，可以使用 `reviewer` 定义——数据隔离照常（sessions.db 在 `coder` 目录下），人格内容来自 `agents/reviewer.md`。

> 代码证据：`agents_def_paths(name)` 刻意不查 agent 层——agent_root 是数据隔离目录，定义库与 agent_id 隔离体系正交。

## Params 三件套：运行时参数

```text
EngineParams {                   // 引擎级，启动时定死
    agent_paths: AgentPaths       // 数据在哪（三层路径）
}

SessionParams {                  // 对话级，创建对话时提供（agent_config 定死，model_config 可运行时切）
    agent_config: AgentConfig {
        definition: String          // 用哪个定义（加载 agents/{definition}.md，必填：未知名报错，出厂人格传 default）
    }
    model_config: ModelConfig {
        model_id: String                  // 用哪个模型（provider_id/model_id，必填非空，未指定则拒绝对话）
        thinking_type: Option<...>        // 思考开关
        reasoning_effort: Option<String>  // 思考强度档位
    }
}
```

UI 层只需在对应动作时提供对应 Params：

- `fuyao_app::start(EngineParams)` 或 `Engine::new(engine_params, ...)`：构造 `EngineParams`
- `engine.create_session(SessionParams)` 或 `engine.resume_session(id, SessionParams)`：构造 `SessionParams`
- `engine.fork_session(source_id, SessionParams)` 或 `engine.create_child_session(parent_id, ChildSessionSource, SessionParams)`：派生 / 子任务场景同样构造 `SessionParams`
- `engine.send(id, InputEvent)`：无 Params——模型已在 session 的 `model_config` 定好；要切模型用 `engine.update_session_params(id, new_params)`，下一轮生效

## agent_id 解析

两种格式（入口：`get_agent_root(agent_id, fuyao_home, workspace)`）：

| 格式 | 语义 | agent_root |
|------|------|-----------|
| `global/{名}` | 强制全局层 | `{fuyao_home}/fuyao-agents/{名}` |
| `workspace/{名}` | 强制工作区层 | `{workspace}/.fuyao/fuyao-agents/{名}` |

数据去向必须显式声明，禁止隐式选址：

- **来源前缀必填**：裸名、未知来源、目录名含斜杠或为空，一律视为格式错误（与 model_id 的 `provider/model` 拆分同构：`/` 前是来源、`/` 后是目录）
- **前缀大小写不敏感**：`Global/coder`、`WORKSPACE/coder` 与小写形式等价命中对应层——列举侧 `AgentIdSource` 的 PascalCase 序列化值可直接作前缀，前端无需大小写转换
- **配对约束**：`workspace/{名}` 必须提供 workspace 参数，否则报错（不再整串隐式落全局层）
- **启动即校验**：引擎装配（`init_engine`）启动时校验，非法输入 fail-fast 报错（`InitError::InvalidAgentId`），错误信息含正确格式与修正建议

> 全局层基准：`~/.fuyao/`，可用 `FUYAO_HOME` 环境变量覆盖。agent_root 路径解析读
> `AgentPaths` 注入的 `fuyao_home` 字段（纯函数、零全局状态）。

## 三层路径系统

### 三层定义

| 层 | 基准路径 | 说明 |
|----|---------|------|
| 全局层 | `~/.fuyao/` | 所有 Agent 共享 |
| Agent 层 | `{agent_root}/` | 独立 Agent 的数据隔离单元（agent_id 存在时） |
| 工作区层 | `{workspace}/.fuyao/` | 项目级配置 |

### 各资源的分布

| 资源 | 全局层 | Agent 层 | 工作区层 |
|------|--------|---------|---------|
| fuyao.toml | ✅ | ✅ | ✅ |
| .env | ✅ | ✅ | ✅ |
| sessions.db | ✅（无 agent_id 时） | ✅ | — |
| 日志 | ✅（无 agent_id 时） | ✅ | — |
| Skills | ✅ | ✅ | ✅ |
| Agent 定义 | ✅ | **—** | ✅ |
| AGENTS.md | ✅ | ✅ | ✅（workspace 根） |
| 补充指令 | ✅ | ✅ | ✅ |

**两个不对称设计**：
1. **sessions.db 和日志**：工作区层不参与。会话数据永远在 Agent 层或全局层，与 agent_id 对齐。
2. **Agent 定义**：Agent 层恒不参与。定义库与 agent_id 隔离体系正交。

## LayeredPaths：三个遍历方法

```text
LayeredPaths {
    global_: Option<PathBuf>    // 全局层
    agent: Option<PathBuf>      // Agent 层
    workspace: Option<PathBuf>  // 工作区层
    extra: Vec<PathBuf>         // 额外目录（插件等，最低优先级）
}
```

三个遍历方法对应三种语义：

| 方法 | 返回 | 语义 | 用途 |
|------|------|------|------|
| `all()` | 按优先级排列的路径列表 | workspace → agent → global → extra | 配置合并 |
| `first_exists()` | 首个存在的路径 | **覆盖**（高优先级胜出） | 单文件查找（agents/{name}.md） |
| `merge_exists()` | 所有存在的路径 | **叠加**（全拼接） | 多层资源合并（AGENTS.md、instructions/、skills/） |

## 关键设计决策

### 为什么 sessions.db 不在工作区层？

会话数据属于独立 Agent 实体，不属于项目。一个 Agent（如 `global/coder`）在不同工作区使用时，会话历史应该连续，不应因切换工作区而丢失。

### 为什么 Agent 定义不查 Agent 层？

agent_root（如 `~/.fuyao/fuyao-agents/coder/`）是数据隔离目录（sessions / config / provider）。Agent 定义（`agents/reviewer.md`）是能力 / 人格描述，与数据隔离正交。如果把定义塞进每个独立 Agent 的数据目录，会导致定义碎片化——修改一个定义需要改 N 个目录。

### 为什么 AgentPaths 注入 fuyao_home 而非读全局函数？

`fuyao_home` 字段在构造时注入，所有路径方法读此字段而非调 `get_fuyao_home()`。这让路径解析成为纯函数——零全局状态，测试可 per-instance 隔离（不同测试用不同 fuyao_home，互不干扰）。

### 为什么强制来源前缀而非自动选址？

agent_id 决定数据（sessions.db、日志、配置）的物理位置，必须无歧义。自动选址让
数据去向取决于磁盘状态（工作区已有目录则复用，否则落全局）——同一个裸名在
不同机器、不同时刻可能指向不同目录，会话历史悄悄分叉且无法追溯。强制前缀把
「数据在哪」变成调用方显式声明的契约：同名文件夹可在全局层与项目层并存且都是
合法目标，前缀是唯一消歧手段；格式错误在引擎启动时即报错，而不是隐式选一个
位置继续跑。
