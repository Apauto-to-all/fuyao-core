---
name: fuyao-config
description: fuyao 引擎配置指南：三层配置（全局 / agent / 工作区）路径与合并优先级、fuyao.toml 全部配置段字段、添加供应商（仅全局层）、创建技能、创建 Agent 定义（智能体）。用户要查看或修改 fuyao 配置、增删供应商、新建技能或智能体、排查配置加载报错时使用。
---

# fuyao 引擎配置

## 三层配置模型

fuyao 的一切落盘资源按三层组织，同名资源高优先级层覆盖低优先级层：

| 层 | 优先级 | 根路径 |
| --- | --- | --- |
| 工作区 workspace | 最高 | `{工作目录}/.fuyao/` |
| agent | 中 | `{agent_root}/`（仅当指定 agent_id 时存在） |
| 全局 global | 最低 | `~/.fuyao/`（环境变量 `FUYAO_HOME` 可重定向） |

每层目录内布局同构：`fuyao.toml`（配置）、`.env`（环境变量）、`skills/`（技能）、`agents/`（Agent 定义）、`instructions/`（指令片段）。

- agent_root 由 agent_id 决定：`global/{名}` → `~/.fuyao/fuyao-agents/{名}/`；`workspace/{名}` → `{工作目录}/.fuyao/fuyao-agents/{名}/`
- fuyao.toml 三层递归深合并：table 逐字段合并，标量与数组整值覆盖（数组不跨层拼接）；`.env` 三层逐层覆盖加载
- 技能与 Agent 定义在三层之外还有插件 extra 层与内置兜底（default / explore / executor 定义、fuyao-config 技能），同名用户文件覆盖内置版本
- 例外：AGENTS.md 与 instructions/ 不是覆盖而是多层**全部叠加**进系统提示词（见 `references/instructions.md`）

## 硬性规则

- `[providers]`（供应商定义）只允许写在全局层 `~/.fuyao/fuyao.toml`，出现在 agent / workspace 层即整体加载失败
- 配置在引擎启动时一次性加载：修改 fuyao.toml 后需重启引擎才生效，providers 是唯一例外（应用层可热刷新）
- `[models]` 段只认 `fast` 标签，配置其他标签直接加载报错；其余配置段的未知键默认忽略
- Agent 定义文件存在但 frontmatter 损坏会直接报错，不会静默跳到下一层

## 按任务取详细参考

详细字段与操作步骤放在关联文件中，按任务用 skill 工具按需读取，如 `skill(name="fuyao-config", file_path="references/config.md")`：

| 任务 | 读取 |
| --- | --- |
| 查看 / 修改 fuyao.toml 任意配置段的字段、类型、默认值 | `references/config.md` |
| 添加供应商（provider 定义、模型、价格、API Key） | `references/providers.md` |
| 创建技能（SKILL.md 规范、frontmatter、关联文件） | `references/skills.md` |
| 创建 Agent 定义 / 智能体（frontmatter、mode、tools 收窄） | `references/agents.md` |
| 给引擎加持久指令 / 项目上下文（AGENTS.md、instructions/） | `references/instructions.md` |
