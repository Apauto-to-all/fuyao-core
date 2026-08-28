//! 统一内置资产模块（编译期嵌入）
//!
//! 承载任意种类的编译期内置资产，当前两种：Agent 定义（`assets/agents/`）与
//! 技能 skills（`assets/skills/`）。资产目录结构与运行时目录约定同构——
//! `assets/agents/{name}.md` 对应运行时 `agents/{name}.md`，
//! `assets/skills/{name}/SKILL.md` 对应运行时 `skills/{name}/SKILL.md`——
//! 后续新增任何内置资源种类都走同一条上车道：在 [`BuiltinKind`] 加变体、
//! 在 [`BUILTIN_ENTRIES`] 加条目。
//!
//! 不变量：
//! - **最低优先级兜底**：内置恒为解析链的最后一环，四层文件系统
//!   （workspace > agent > global > extra）同名资产覆盖内置版本
//! - **零落盘**：全部资产经 [`include_str!`] 编译期直达内存，运行时零读盘、
//!   零资产依赖；升级即重编译，内容随之刷新
//! - **单一事实源**：名字清单与查找都从 [`BUILTIN_ENTRIES`] 静态表派生，
//!   无第二份清单可漂移，故无需清单一致性测试
//! - **Agent 定义资产必须带 mode**：primary / subagent 的区分由 frontmatter
//!   承载，不靠目录
//! - **Skills 条目必有 SKILL.md**：`files` 首项即 `SKILL.md`，其余为
//!   Tier 3 关联文件（按 `scripts/` / `references/` / `assets/` 子目录组织）

/// 内置资产种类：与运行时目录约定一一对应（`assets/agents/`、`assets/skills/`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuiltinKind {
    /// Agent 定义：单文件资产 `assets/agents/{name}.md`
    Agents,
    /// 技能：目录资产 `assets/skills/{name}/SKILL.md`（+ 可选关联文件）
    Skills,
}

/// 单个内置资产条目：名字 + 目录内相对路径 → 内容 的文件集
pub(crate) struct BuiltinEntry {
    /// 资产种类
    kind: BuiltinKind,
    /// 资产名（Agents = 文件 stem；Skills = 目录名）
    name: &'static str,
    /// 文件集：`Assets` 种类下单文件（`{name}.md`）；`Skills` 种类下首项必为
    /// `SKILL.md`，其余为按关联子目录组织的 Tier 3 文件
    files: &'static [(&'static str, &'static str)],
}

impl BuiltinEntry {
    /// 资产名
    pub(crate) fn name(&self) -> &'static str {
        self.name
    }

    /// 按目录内相对路径索引文件内容
    ///
    /// 相对路径只与静态表中的字面量精确匹配，无文件系统路径可逃逸，
    /// 结构性免疫路径穿越。
    pub(crate) fn file(&self, rel: &str) -> Option<&'static str> {
        self.files
            .iter()
            .find(|(path, _)| *path == rel)
            .map(|(_, content)| *content)
    }

    /// 全部文件集（相对路径 → 内容），供调用方按需推导结构化信息
    pub(crate) fn files(&self) -> &'static [(&'static str, &'static str)] {
        self.files
    }
}

/// 内置资产全集：新增内置资产在此追加条目
const BUILTIN_ENTRIES: &[BuiltinEntry] = &[
    // Agents：单文件资产，files 只有一项（文件名即 {name}.md）
    BuiltinEntry {
        kind: BuiltinKind::Agents,
        name: "default",
        files: &[("default.md", include_str!("assets/agents/default.md"))],
    },
    BuiltinEntry {
        kind: BuiltinKind::Agents,
        name: "explore",
        files: &[("explore.md", include_str!("assets/agents/explore.md"))],
    },
    BuiltinEntry {
        kind: BuiltinKind::Agents,
        name: "executor",
        files: &[("executor.md", include_str!("assets/agents/executor.md"))],
    },
    // Skills：目录资产，SKILL.md 必有，其余为 Tier 3 关联文件
    BuiltinEntry {
        kind: BuiltinKind::Skills,
        name: "fuyao-config",
        files: &[
            (
                "SKILL.md",
                include_str!("assets/skills/fuyao-config/SKILL.md"),
            ),
            (
                "references/config.md",
                include_str!("assets/skills/fuyao-config/references/config.md"),
            ),
            (
                "references/instructions.md",
                include_str!("assets/skills/fuyao-config/references/instructions.md"),
            ),
            (
                "references/providers.md",
                include_str!("assets/skills/fuyao-config/references/providers.md"),
            ),
            (
                "references/skills.md",
                include_str!("assets/skills/fuyao-config/references/skills.md"),
            ),
            (
                "references/agents.md",
                include_str!("assets/skills/fuyao-config/references/agents.md"),
            ),
        ],
    },
];

/// 列举指定种类的全部内置资产名（与注册表条目顺序一致）
pub(crate) fn builtin_names(kind: BuiltinKind) -> Vec<&'static str> {
    BUILTIN_ENTRIES
        .iter()
        .filter(|entry| entry.kind == kind)
        .map(|entry| entry.name)
        .collect()
}

/// 按种类与名字取内置资产条目
///
/// 仅精确匹配（空串名不匹配任何条目），未命中返回 `None`，
/// 由调用方决定错误语义。
pub(crate) fn builtin_entry(kind: BuiltinKind, name: &str) -> Option<&'static BuiltinEntry> {
    BUILTIN_ENTRIES
        .iter()
        .find(|entry| entry.kind == kind && entry.name == name)
}

/// 按名字取内置 Agent 定义的原始 Markdown 文本
///
/// Agents 种类的糖：单文件资产取 `files` 首项内容。
/// 覆盖链的一环：用户 `agents/{name}.md` 不存在时查本表。
pub(crate) fn builtin_agent_md(name: &str) -> Option<&'static str> {
    builtin_entry(BuiltinKind::Agents, name).map(|entry| entry.files[0].1)
}

/// 按技能名与相对路径取内置技能的文件内容（Tier 3 按需加载同口径）
///
/// 相对路径只与静态表字面量精确匹配，无文件系统路径可逃逸，
/// 结构性免疫路径穿越。
pub(crate) fn builtin_skill_file(name: &str, rel: &str) -> Option<&'static str> {
    builtin_entry(BuiltinKind::Skills, name).and_then(|entry| entry.file(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Agents 名单含全部内置 Agent 定义
    #[test]
    fn builtin_names_agents_contains_all() {
        let names = builtin_names(BuiltinKind::Agents);
        assert_eq!(names, vec!["default", "explore", "executor"]);
    }

    /// Skills 名单含全部内置技能
    #[test]
    fn builtin_names_skills_contains_all() {
        let names = builtin_names(BuiltinKind::Skills);
        assert_eq!(names, vec!["fuyao-config"]);
    }

    /// 名字清单直接从注册表派生：总数 = 各种类条目数之和，无第二份清单
    #[test]
    fn builtin_names_cover_every_entry() {
        let total: usize = [BuiltinKind::Agents, BuiltinKind::Skills]
            .iter()
            .map(|kind| builtin_names(*kind).len())
            .sum();
        assert_eq!(total, BUILTIN_ENTRIES.len());
    }

    /// 主定义必须声明 primary 模式（mode 区分由 frontmatter 承载）
    #[test]
    fn builtin_agent_md_default_is_primary() {
        let md = builtin_agent_md("default").expect("default 应有内置定义");
        assert!(md.contains("mode: primary"));
    }

    /// 子代理定义必须声明 subagent 模式
    #[test]
    fn builtin_agent_md_explore_is_subagent() {
        let md = builtin_agent_md("explore").expect("explore 应有内置定义");
        assert!(md.contains("mode: subagent"));
    }

    #[test]
    fn builtin_agent_md_executor_exists() {
        let md = builtin_agent_md("executor").expect("executor 应有内置定义");
        assert!(md.contains("mode: subagent"));
    }

    /// 未知名与空串不匹配任何条目
    #[test]
    fn builtin_agent_md_unknown_and_empty_names_miss() {
        assert!(builtin_agent_md("nonexistent").is_none());
        assert!(builtin_agent_md("").is_none());
    }

    /// builtin_entry 空串名不匹配任何条目（跨种类）
    #[test]
    fn builtin_entry_rejects_empty_name() {
        assert!(builtin_entry(BuiltinKind::Agents, "").is_none());
        assert!(builtin_entry(BuiltinKind::Skills, "").is_none());
    }

    /// builtin_entry 按种类隔离：Agent 名在 Skills 种类下不命中，反之亦然
    #[test]
    fn builtin_entry_isolates_kinds() {
        assert!(builtin_entry(BuiltinKind::Skills, "default").is_none());
        assert!(builtin_entry(BuiltinKind::Agents, "fuyao-config").is_none());
    }

    /// 内置技能 SKILL.md 命中
    #[test]
    fn builtin_skill_file_hits_skill_md() {
        let md = builtin_skill_file("fuyao-config", "SKILL.md").expect("应命中 SKILL.md");
        assert!(md.contains("name: fuyao-config"));
    }

    /// 未知名 / 未知文件路径 → None
    #[test]
    fn builtin_skill_file_misses_unknowns() {
        assert!(builtin_skill_file("nonexistent", "SKILL.md").is_none());
        assert!(builtin_skill_file("fuyao-config", "no-such-file.md").is_none());
        assert!(builtin_skill_file("", "SKILL.md").is_none());
    }

    /// 每个 Skills 条目必含 SKILL.md：缺失即无法被技能发现层识别
    #[test]
    fn every_skills_entry_has_skill_md() {
        for name in builtin_names(BuiltinKind::Skills) {
            assert!(
                builtin_skill_file(name, "SKILL.md").is_some(),
                "内置技能 {name} 缺 SKILL.md"
            );
        }
    }

    /// 每个 Agents 条目必含恰好一个文件且文件名为 {name}.md
    #[test]
    fn every_agents_entry_is_single_named_file() {
        for entry in BUILTIN_ENTRIES
            .iter()
            .filter(|e| e.kind == BuiltinKind::Agents)
        {
            assert_eq!(entry.files.len(), 1, "Agent 定义资产应是单文件");
            assert_eq!(entry.files[0].0, format!("{}.md", entry.name));
        }
    }
}
