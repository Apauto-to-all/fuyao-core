//! 系统提示词分层构建函数
//!
//! 各层 section 的构建逻辑，按顺序拼接成最终系统提示词。
//! 定义与技能数据的获取见 `definitions` 与 `skills` 模块，本模块只做格式化。
//!
//! 分层结构（6层，与 builder 的拼装顺序一致）：
//! | Layer | 内容 | 当前状态 |
//! |-------|------|----------|
//! | 1 | Agent 身份 | ✅ 已实现 |
//! | 2 | 项目上下文 | ✅ 已实现 |
//! | 3 | Skills 索引 | ✅ 已实现 |
//! | 4 | 子代理索引（仅主 Agent 注入） | ✅ 已实现 |
//! | 5 | 补充指令 | ✅ 已实现 |
//! | 6 | 环境（日期时间 + 运行环境合为一节） | ✅ 已实现 |

use chrono::Local;
use fuyao_api::{AgentDefinition, AgentPaths};
use std::path::PathBuf;

/// 构建 Agent 身份 section（Layer 1）
///
/// `definition` 已由调用方经 [`crate::resolve_definition`] 加载（含 mode 校验），
/// 本函数仅取其 `system_prompt`。加载与校验逻辑统一收口于 [`crate::resolve_definition`]。
pub fn build_agent_identity_section(definition: &AgentDefinition) -> String {
    definition.system_prompt.clone()
}

/// 构建项目上下文 section（Layer 2）
///
/// AGENTS.md 按三层架构加载，优先级从高到低：
/// 1. 工作目录层：{workspace}/AGENTS.md（最高优先级）
/// 2. Agent 目录层：{agent}/AGENTS.md（次高优先级）
///
/// 使用 agent_paths.agents_md_paths 获取分层路径，merge_exists() 获取存在的路径。
pub fn build_project_context_section(agent_paths: &AgentPaths) -> String {
    let paths = agent_paths.agents_md_paths();

    // 获取所有存在的路径（按优先级排序）
    let existing_paths = paths.merge_exists();

    if existing_paths.is_empty() {
        return String::new();
    }

    let mut contexts: Vec<(&str, String)> = Vec::new(); // (层级名称, 内容)

    for path in &existing_paths {
        if let Ok(content) = std::fs::read_to_string(path) {
            let content = content.trim().to_string();
            if !content.is_empty() {
                // 根据路径判断层级
                let layer_name = if paths.workspace.as_ref() == Some(path) {
                    "项目层"
                } else if paths.agent.as_ref() == Some(path) {
                    "Agent 层"
                } else {
                    "全局层"
                };
                contexts.push((layer_name, content));
            }
        }
    }

    if contexts.is_empty() {
        return String::new();
    }

    // 拼接各层内容，用子标题区分来源
    let parts: Vec<String> = contexts
        .iter()
        .map(|(layer_name, content)| format!("## {layer_name}\n\n{content}"))
        .collect();

    parts.join("\n\n")
}

/// 构建补充指令 section（Layer 5）
///
/// 扫描 `instructions/` 文件夹（四层优先级：workspace → agent → global → extra），
/// 每个目录下所有 `*.md` 全量拼接，每个文件用 `## {完整路径}` 作标题区分。
///
/// 文件格式为纯 md 正文（不支持 frontmatter）。
/// 空内容则返回空字符串（section 被跳过）。
pub fn build_instructions_section(agent_paths: &AgentPaths) -> String {
    let paths = agent_paths.instructions_paths();
    let existing_dirs = paths.merge_exists();

    if existing_dirs.is_empty() {
        return String::new();
    }

    // 收集所有 (完整路径, 正文) 条目，按目录优先级 + 目录内文件名排序
    let mut entries: Vec<(String, String)> = Vec::new();

    for dir in &existing_dirs {
        // 读目录下所有 *.md 文件
        let mut md_files: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file() && p.extension().is_some_and(|ext| ext == "md"))
                .collect(),
            Err(_) => continue,
        };
        // 目录内按文件名排序
        md_files.sort();

        for file_path in md_files {
            if let Ok(content) = std::fs::read_to_string(&file_path) {
                let content = content.trim().to_string();
                if !content.is_empty() {
                    entries.push((file_path.to_string_lossy().to_string(), content));
                }
            }
        }
    }

    if entries.is_empty() {
        return String::new();
    }

    // 每个文件用完整路径作标题，正文拼接
    let parts: Vec<String> = entries
        .iter()
        .map(|(path, content)| format!("## {path}\n\n{content}"))
        .collect();

    parts.join("\n\n")
}

/// 构建 Skills 索引 section（Layer 3）
///
/// 列出可用的 Skills 名称和摘要，供 Agent 快速了解可用技能。
/// 数据来自 [`crate::skills::list_skills`]：文件系统四层发现 + 内置技能
/// 兜底（内置技能从此进入系统提示词技能索引）。
pub fn build_skills_section(agent_paths: &AgentPaths) -> String {
    let all_skills = match crate::skills::list_skills(agent_paths) {
        Ok(skills) => skills,
        Err(_) => return String::new(),
    };

    if all_skills.is_empty() {
        return String::new();
    }

    let mut lines = vec!["可用 Skills：".to_string()];
    for skill in &all_skills {
        let desc = if skill.description.is_empty() {
            String::new()
        } else {
            format!("：{}", skill.description)
        };
        lines.push(format!("- {}{}", skill.name, desc));
    }

    lines.join("\n")
}

/// 构建子代理索引 section（Layer 4）
///
/// 列出可用的子代理定义（name + description），供主 Agent 通过 `subagent` 工具的
/// `subagent_type` 参数选择。仅注入主 Agent session（子代理不可再派生）。
/// 数据来自 [`crate::list_subagent_definitions`]，空列表返回空字符串（section 被跳过）。
pub fn build_subagent_index_section(agent_paths: &AgentPaths) -> String {
    let entries = crate::definitions::list_subagent_definitions(agent_paths);

    if entries.is_empty() {
        return String::new();
    }

    let mut lines = vec!["可用子代理（subagent 工具的 subagent_type 参数可选值）：".to_string()];
    for opt in &entries {
        let desc = if opt.definition.description.is_empty() {
            String::new()
        } else {
            format!("：{}", opt.definition.description)
        };
        lines.push(format!("- {}{desc}", opt.id));
    }

    lines.join("\n")
}

/// 构建日期时间 section（Layer 6 环境层，与运行环境合为一节）
///
/// 提供当前日期时间信息。
pub fn build_datetime_section() -> String {
    let now = Local::now();
    format!("当前时间：{}", now.format("%Y-%m-%d %H:%M"))
}

/// 构建运行环境 section（Layer 6 环境层，与日期时间合为一节）
///
/// 显示操作系统信息，帮助 Agent 了解运行环境。
pub fn build_environment_section() -> String {
    format!("运行环境：{}", std::env::consts::OS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_datetime_section_works() {
        let section = build_datetime_section();
        assert!(section.contains("当前时间："));
    }

    #[test]
    fn build_environment_section_works() {
        let section = build_environment_section();
        assert!(section.contains("运行环境："));
    }

    #[test]
    fn build_skills_section_includes_builtin_skill() {
        // 默认无文件系统 skills 目录 → 内置技能 fuyao-config 仍进入技能索引
        let ctx = AgentPaths::default();
        let section = build_skills_section(&ctx);
        assert!(section.contains("fuyao-config"), "应含内置技能：{section}");
    }

    #[test]
    fn build_subagent_index_section_lists_builtins() {
        // 默认无用户 agents 目录 → 仅列内置 explore / executor
        let ctx = AgentPaths::default();
        let section = build_subagent_index_section(&ctx);
        assert!(section.contains("explore"));
        assert!(section.contains("executor"));
        assert!(section.contains("只读探索"));
        assert!(section.contains("通用执行"));
    }

    #[test]
    fn build_subagent_index_section_user_overrides_builtin() {
        // 用户 agents/explore.md 覆盖内置 explore 的 description
        let temp = std::env::temp_dir().join("fuyao_test_subagent_index_override");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        std::fs::write(
            plugin.join("agents").join("explore.md"),
            "---\nname: explore\ndescription: 我的自定义探索\nmode: subagent\n---\n自定义",
        )
        .unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let section = build_subagent_index_section(&ctx);
        assert!(section.contains("我的自定义探索"));
        // 内置描述被覆盖，不再出现
        assert!(!section.contains("只读探索"));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn build_subagent_index_section_filters_primary_only_defs() {
        // mode: primary 的定义不应出现在子代理索引
        let temp = std::env::temp_dir().join("fuyao_test_subagent_index_filter");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        std::fs::write(
            plugin.join("agents").join("boss.md"),
            "---\nname: boss\ndescription: 专属主代理\nmode: primary\n---\n仅主代理",
        )
        .unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let section = build_subagent_index_section(&ctx);
        assert!(!section.contains("boss"));
        assert!(!section.contains("专属主代理"));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn build_instructions_section_empty_for_default() {
        // 默认无 instructions 目录，应返回空
        let ctx = AgentPaths::default();
        let section = build_instructions_section(&ctx);
        assert!(section.is_empty());
    }

    #[test]
    fn build_instructions_section_loads_multiple_md() {
        // 通过 extra_dirs 注入 instructions/ 下两个 md，验证全量拼接 + 完整路径标题 + 排序
        let temp = std::env::temp_dir().join("fuyao_test_instructions_section");
        let plugin = temp.join("plugin");
        let instr_dir = plugin.join("instructions");
        std::fs::create_dir_all(&instr_dir).unwrap();
        std::fs::write(instr_dir.join("b-git.md"), "Git 规范内容").unwrap();
        std::fs::write(instr_dir.join("a-rust.md"), "Rust 规范内容").unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let section = build_instructions_section(&ctx);

        // 非空，包含两个文件的正文
        assert!(!section.is_empty());
        assert!(section.contains("Rust 规范内容"));
        assert!(section.contains("Git 规范内容"));
        // 按文件名排序：a-rust 在 b-git 前
        let rust_pos = section.find("Rust 规范内容").unwrap();
        let git_pos = section.find("Git 规范内容").unwrap();
        assert!(rust_pos < git_pos);
        // 标题用完整路径（包含 a-rust.md）
        assert!(section.contains("a-rust.md"));
        assert!(section.contains("b-git.md"));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn build_instructions_section_skips_empty_md() {
        // 空内容的 md 文件应被跳过
        let temp = std::env::temp_dir().join("fuyao_test_instructions_empty");
        let plugin = temp.join("plugin");
        let instr_dir = plugin.join("instructions");
        std::fs::create_dir_all(&instr_dir).unwrap();
        std::fs::write(instr_dir.join("empty.md"), "   \n\n  ").unwrap();
        std::fs::write(instr_dir.join("real.md"), "有内容").unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let section = build_instructions_section(&ctx);

        assert!(section.contains("有内容"));
        assert!(!section.contains("empty.md"));

        std::fs::remove_dir_all(&temp).ok();
    }
}
