# 问题追踪器：本地 Markdown

本仓库的 issue 与规格文档以 Markdown 文件形式存放于 `.scratch/` 目录。

## 约定

- 一个特性一个目录：`.scratch/<feature-slug>/`
- 规格文档为 `.scratch/<feature-slug>/spec.md`
- 实现 issue 一票一文件：`.scratch/<feature-slug>/issues/<NN>-<slug>.md`，编号从 `01` 起——禁止把多张票合并进单个文件
- 分诊状态记录在 issue 文件顶部附近的 `Status:` 行（角色字符串见 `triage-labels.md`）
- 评论与对话历史追加到文件底部 `## Comments` 标题下

## 当技能说「发布到问题追踪器」时

在 `.scratch/<feature-slug>/` 下创建新文件（目录不存在则先创建）。

## 当技能说「获取相关工单」时

读取所引用路径的文件。用户通常会直接传入路径或 issue 编号。

## Wayfinder 操作

供 `/wayfinder` 使用。**地图（map）**是一个文件，每张子票对应一个子文件。

- **地图**：`.scratch/<effort>/map.md`——包含 Notes / Decisions-so-far / Fog 正文
- **子票**：`.scratch/<effort>/issues/NN-<slug>.md`，编号从 `01` 起，正文写问题。`Type:` 行记录票类型（`research`/`prototype`/`grilling`/`task`）；`Status:` 行记录 `claimed`/`resolved`
- **阻塞**：文件顶部附近一行 `Blocked by: NN, NN`。所列文件全部 `resolved` 后该票解除阻塞
- **前沿（frontier）**：扫描 `.scratch/<effort>/issues/` 中处于打开、未阻塞、未认领状态的文件；编号最小者优先
- **认领**：动手前先置 `Status: claimed` 并保存
- **解决**：在 `## Answer` 标题下追加答案，置 `Status: resolved`，再向 `map.md` 的 Decisions-so-far 追加一条上下文指针（要点 + 链接）
