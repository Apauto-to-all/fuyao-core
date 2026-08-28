# 创建技能

技能是 Agent Skills 协议资产：一个目录 + 一份 `SKILL.md`（frontmatter + 正文），可携带关联文件。Agent 运行时经 `skill` 工具三层读取：无参列举（Tier 1）→ 按名加载完整定义（Tier 2）→ 按名 + 文件路径读关联文件（Tier 3）。

## 目录规范与优先级

| 层 | 路径 | 适用 |
| --- | --- | --- |
| workspace | `{工作目录}/.fuyao/skills/{名}/SKILL.md` | 项目专属技能 |
| agent | `{agent_root}/skills/{名}/SKILL.md` | 独立 Agent 私有（仅指定 agent_id 时参与） |
| global | `~/.fuyao/skills/{名}/SKILL.md` | 个人全局技能 |
| extra | `{插件根}/skills/{名}/SKILL.md` | 插件分发 |
| 内置 | 编译期嵌入（当前含 fuyao-config） | 兜底，恒为最低优先级 |

- 同名技能高优先级层胜出，列举与加载口径一致
- 技能名 = SKILL.md 的**父目录名**（目录内可再嵌套分组目录，按 SKILL.md 向上取最近目录名）
- 扫描排除目录：`.git` `.github` `.venv` `venv` `.env` `node_modules` `__pycache__` `dist` `build` 与隐藏目录——技能目录避开这些名字

## frontmatter 字段

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `name` | 是 | 技能名（缺失时回退目录名；超 64 字符截断） |
| `description` | 是 | 触发说明（缺失时回退正文首行非标题；超 1024 字符截断）。**这是模型决定是否使用技能的唯一线索**，写清「何时用、覆盖哪些触发场景」 |
| `license` | 否 | 许可证 |
| `compatibility` | 否 | 兼容性说明（超 500 字符截断） |
| `metadata` | 否 | 任意键值对（YAML mapping） |

frontmatter 的 YAML 解析是宽松的：语法错误不报错，`---` 分隔与正文照常提取，仅 frontmatter 字段回退为空值——务必保证分隔符与 YAML 语法正确，否则 name/description 会静默丢失。

## 关联文件（Tier 3）

- 仅 `scripts/`、`references/`、`assets/` 三个子目录计入关联文件清单（只扫直接文件，不递归）
- 根级散放文件不进清单，但按路径仍可读取；详细文档放 `references/`、可执行脚本放 `scripts/`
- SKILL.md 正文用相对路径引用，模型按需读取：`skill(name="技能名", file_path="references/xxx.md")`

## 创建步骤

1. **选层**：项目用 → workspace；个人通用 → global；独立 Agent 私有 → agent 层
2. **建目录** `{层}/skills/{名}/`：名用小写连字符（无格式强制校验，保持风格一致）
3. **写 SKILL.md**：frontmatter 带全 `name` 与 `description`，正文写核心流程与规则
4. **拆详情**：主文件只放每次使用都需要的内容，细节拆到 `references/` 关联文件并在正文留取用指针
5. **验证**：`skill()` 列表可见新技能、`skill(name="…")` 完整加载、`skill(name="…", file_path="…")` 关联文件可读
