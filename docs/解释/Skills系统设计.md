# Skills 系统设计

> 本文解释 Skills 的三层渐进披露、目录发现、按需加载与 SKILL.md 格式。API 签名见 `cargo doc --workspace`。

## 三层渐进披露

对应 Agent Skills 官方规范的 Progressive Disclosure——按需加载，省 token：

| Tier | 时机 | 加载内容 | 函数 | 开销 |
|------|------|---------|------|------|
| 1 | 列表（`skill` 不传参） | 仅 name + description | `find_all_skills` | ~100 token/skill |
| 2 | 查看（`skill(name)`） | 完整 frontmatter + body + 关联文件清单 | `load_skill` | body 全量 |
| 3 | 按需（`skill(name, file_path)`） | 单个关联文件内容 | `load_skill_file` | 单文件 |

> **token 经济性**：Tier 1 只读前 4000 字节提取元数据，不加载 body。body 只在 Tier 2 全量读取，关联文件只在 Tier 3 按需读取。

## 目录发现

### 四层目录

`AgentPaths::skills_paths()` 返回 LayeredPaths，四层：

| 层 | 路径 | 优先级 |
|----|------|--------|
| workspace | `{workspace}/.fuyao/skills` | 最高 |
| agent | `{agent_root}/skills` | 高 |
| global | `~/.fuyao/skills` | 中 |
| extra | `{插件根}/skills` | 最低 |

### 扫描与去重

`find_all_skills` 用 `merge_exists()` 取所有存在的目录，按优先级遍历：

- 用 `ignore::WalkBuilder` 递归找 `SKILL.md`（排除隐藏目录 + `.git` / `node_modules` 等）
- **去重**：`HashSet<String>` 先到先得，高优先级层的同名 Skill 覆盖低优先级层
- 结果按 name 字典序排序

`find_skill_md_by_name` 按名查找：先直接匹配 `{name}/SKILL.md`（快路径），未命中再递归搜索，高优先级层找到即返回。

## SKILL.md 格式

```markdown
---
name: pdf-processing
description: Extracts text from PDF files. Use when working with PDF documents.
license: Apache-2.0
compatibility: Requires Python 3.14+ and uv
metadata:
  author: example-org
  version: "1.0"
---

（正文作为 body，Skill 的 Markdown 指令）
```

### frontmatter 字段

| 字段 | 必填 | 缺省 fallback | 截断上限 |
|------|------|-------------|---------|
| `name` | 是 | 父目录名 | 64 字符 |
| `description` | 是 | 正文第一个非标题非空行 | 1024 字符 |
| `license` | 否 | None | — |
| `compatibility` | 否 | None | 500 字符 |
| `metadata` | 否 | 空 HashMap | — |

> 只有 body 进系统提示词的 Skills 列表（name + description），完整 body 经 LLM 按需加载。

### 关联文件

三个固定子目录（`LINKED_SUBDIRS`）：

| 子目录 | 用途 |
|--------|------|
| `scripts/` | 可执行代码 |
| `references/` | 额外文档 |
| `assets/` | 静态资源 |

只扫描顶层文件（不递归），返回相对路径。

## 按需加载

### load_skill（Tier 2）

完整读取 SKILL.md → 解析 frontmatter → fallback name/description → 验证 name 和 description 都有值 → 扫描关联文件 → 截断超长字段 → 返回 SkillDefinition。

### load_skill_file（Tier 3）

加载 Skill 目录下的任意文件，**三重路径遍历防护**：空路径拒绝 / 含 `..` 拒绝 / canonicalize 后必须 starts_with(skill_dir)。

二进制文件降级：读取失败时返回 `[二进制文件: {name}, 大小: {size} 字节]`，不报错。

## skill 工具

LLM 通过 `skill` 工具加载 Skill（见 [工具系统设计](工具系统设计.md)）：

| 调用 | 模式 | 返回 |
|------|------|------|
| `skill()` | 列表 | 所有 Skill 的 name + description |
| `skill(name)` | 查看 | 完整 body + frontmatter + 关联文件清单 |
| `skill(name, file_path)` | 文件 | 单个关联文件内容 |

错误降级：Skill 未找到时列出可用 Skill 名帮 Agent 决策；文件未找到时列出该 Skill 的关联文件清单。

## 与官方规范的关系

项目符合 [Agent Skills 规范](https://agentskills.io/specification) 的核心要求：目录结构、frontmatter、渐进披露、关联子目录、按需加载。

**实现扩展**（规范未限定，不冲突）：四层目录发现、name/description fallback 容错、去重覆盖、二进制降级。

**未实现**：`allowed-tools` 实验性字段（未解析）、name 格式校验（仅长度截断）、name 必须匹配目录名（frontmatter name 优先）。

## 关键设计决策

### 为什么 Tier 1 只读前 4000 字节？

性能。`find_all_skills` 可能扫描几十个 SKILL.md，如果每个都全量读取再解析，启动慢且浪费。前 4000 字节足以覆盖 frontmatter（name + description），body 在 Tier 2 才需要。

### 为什么去重用先到先得而非报错？

同名 Skill 在多层目录是正常场景（workspace 覆盖 global 的同名 Skill 是合理用法）。先到先得 + 按优先级遍历 = 高优先级覆盖，语义清晰。报错反而会阻碍覆盖用法。

### 为什么路径遍历用三重防护？

关联文件路径来自 LLM（不可信输入）。三重防护确保 LLM 无法通过 `../` 越界读取 Skill 目录外的文件。canonicalize 后 starts_with 是最终兜底——即使前两层漏了，这层也拦住。

### 为什么二进制文件降级而非报错？

LLM 可能尝试读取图片、PDF 等二进制关联文件。报错让 Agent 卡住；降级返回元信息（文件名 + 大小）让 Agent 自主决策——"这是二进制文件，我应该换个方式处理"。
