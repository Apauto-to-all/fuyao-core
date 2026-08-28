# AGENTS.md 与 instructions/：系统提示词补充资源

两者都是「写进文件、进系统提示词」的持久指令通道，与 fuyao.toml 配置无关。关键共性：系统提示词在**会话创建时构建一次并冻结落库**——改文件对已有会话不生效，创建新会话才能看到新内容（上下文压缩成功后也会按当前文件重建一次）。

## 分工

| 资源 | 定位 | 注入位置 |
| --- | --- | --- |
| AGENTS.md | 项目上下文：这个项目是什么、怎么干 | 系统提示词「项目上下文」节 |
| instructions/*.md | 行为规则：引擎该怎么表现 | 系统提示词「补充指令」节 |

## AGENTS.md：三层全部叠加

| 层 | 路径 |
| --- | --- |
| workspace | `{工作目录}/AGENTS.md`（在工作区根，不在 .fuyao 下） |
| agent | `{agent_root}/AGENTS.md` |
| global | `~/.fuyao/AGENTS.md` |

- 三层**全部叠加**，不是高覆盖低：每层非空内容以「## 项目层 / ## Agent 层 / ## 全局层」子标题拼接进同一节
- 纯 markdown 全文注入，无 frontmatter 约定；空文件跳过
- 无插件 extra 层（这点与 skills / instructions 不同）

## instructions/：四层全部叠加

| 层 | 路径 |
| --- | --- |
| workspace | `{工作目录}/.fuyao/instructions/` |
| agent | `{agent_root}/instructions/` |
| global | `~/.fuyao/instructions/` |
| extra | `{插件根}/instructions/` |

- 每个目录只扫**顶层 `*.md`**（子目录忽略），目录内按文件名排序
- 纯 markdown 正文（不支持 frontmatter），空文件跳过
- 每个文件以 `## {完整路径}` 为标题**全文**注入，无大小与数量限额——大文件直接撑大系统提示词，规则要写得精炼

## 使用要点

- 项目专属规则 → workspace 层；独立 Agent 私有规则 → agent 层；全局行为偏好 → global 层
- 项目事实放 AGENTS.md、行为规则放 instructions/，两通道都会全文进系统提示词，避免同一条规则写两处
- 改动后创建新会话验证；旧会话持冻结版本继续跑完
