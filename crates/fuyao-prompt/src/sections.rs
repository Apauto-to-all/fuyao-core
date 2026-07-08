//! 系统提示词分层构建函数
//!
//! 各层 section 的构建逻辑，按顺序拼接成最终系统提示词。
//!
//! 分层结构（7层）：
//! | Layer | 内容 | 当前状态 |
//! |-------|------|----------|
//! | 1 | Agent 身份 | ✅ 已实现 |
//! | 2 | 项目上下文 | ✅ 已实现 |
//! | 3 | 工具使用引导 | ✅ 已实现 |
//! | 4 | Memory 快照 | ⏳ Stub（未来功能） |
//! | 5 | Skills 索引 | ✅ 已实现 |
//! | 6 | 日期时间 | ✅ 已实现 |
//! | 7 | 运行环境 | ✅ 已实现 |

use crate::loader::load_agent_definition_from_agent_paths;
use chrono::Local;
use fuyao_api::AgentPaths;
use fuyao_skills::find_all_skills;
use std::path::PathBuf;

/// 构建 Agent 身份 section（Layer 1）
///
/// 从 `agents/default.md` 加载系统提示词（本期固定加载默认定义）。
pub fn build_agent_identity_section(agent_paths: &AgentPaths) -> String {
    let agent_def = load_agent_definition_from_agent_paths(agent_paths);
    agent_def.system_prompt
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

/// 构建补充指令 section
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

/// 构建工具使用引导 section（Layer 3）
///
/// 提供工具使用的最佳实践和策略指导。
/// 工具的具体参数和功能由 API schema 提供，此处只讲使用原则。
pub fn build_tool_guidance_section() -> String {
    let mut guides: Vec<(&str, String)> = Vec::new();

    let file_guidance = "\
- 修改文件必须先 read 确认现有内容，不要凭记忆修改
- 大文件用 offset 和 limit 分段读取，不要一次全读浪费上下文
- 局部修改用 edit（查找替换），整体重写用 write
- edit 的 old_string 要包含足够上下文确保唯一匹配
- search 先定位目标位置，read 再确认上下文，edit 最后修改"
        .to_string();
    guides.push(("文件操作", file_guidance));

    let command_guidance = "\
- bash 用于构建、安装、git 操作、运行脚本等需要 shell 的场景
- 文件操作优先使用专用工具（read/write/edit/search），不要用 bash 的 cat/echo/grep
- 长时间命令设置合理的 timeout（默认 120 秒，长任务设 300）"
        .to_string();
    guides.push(("命令执行", command_guidance));

    let efficiency_guidance = "\
- 多个独立的读取操作可以并行调用
- 复杂多步骤任务用 todowrite 拆解并跟踪进度
- 不确定有哪些 Skills 时，用 skill() 浏览可用的技能"
        .to_string();
    guides.push(("效率原则", efficiency_guidance));

    if guides.is_empty() {
        return String::new();
    }

    let parts: Vec<String> = guides
        .iter()
        .map(|(category, content)| format!("## {category}\n\n{content}"))
        .collect();

    parts.join("\n\n")
}

/// 构建 Memory 快照 section（Layer 4）
///
/// 从 Memory store 获取快照。
/// 当前为 stub，未来功能。
pub fn build_memory_section() -> String {
    String::new()
}

/// 构建 Skills 索引 section（Layer 5）
///
/// 列出可用的 Skills 名称和摘要，供 Agent 快速了解可用技能。
pub fn build_skills_section(agent_paths: &AgentPaths) -> String {
    let all_skills = match find_all_skills(agent_paths) {
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

/// 构建日期时间 section（Layer 6）
///
/// 提供当前日期时间信息。
pub fn build_datetime_section() -> String {
    let now = Local::now();
    format!("当前时间：{}", now.format("%Y-%m-%d %H:%M"))
}

/// 构建运行环境 section（Layer 7）
///
/// 显示操作系统信息，帮助 Agent 了解运行环境。
pub fn build_environment_section() -> String {
    format!("运行环境：{}", std::env::consts::OS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_tool_guidance_section_works() {
        let section = build_tool_guidance_section();
        assert!(!section.is_empty());
        assert!(section.contains("文件操作"));
        assert!(section.contains("命令执行"));
        assert!(section.contains("效率原则"));
    }

    #[test]
    fn build_memory_section_returns_empty() {
        let section = build_memory_section();
        assert!(section.is_empty());
    }

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
    fn build_skills_section_empty_for_default() {
        let ctx = AgentPaths::default();
        let section = build_skills_section(&ctx);
        // 默认没有 skills 目录，应该返回空
        assert!(section.is_empty());
    }

    #[test]
    fn build_agent_identity_section_returns_default() {
        let ctx = AgentPaths::default();
        let section = build_agent_identity_section(&ctx);
        assert!(!section.is_empty());
        assert!(section.contains("Fuyao"));
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
