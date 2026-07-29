//! 系统提示词组装器
//!
//! 负责分层组装系统提示词，明确分为「覆盖区 + 补充区」两部分：
//! - 覆盖区：Agent 定义（`agents/default.md` 正文，覆盖内置默认）
//! - 补充区：项目上下文（AGENTS.md）+ 补充指令（`instructions/` 文件夹）
//!
//! 系统提示词由 Agent 模块管理，会话创建时构建一次并冻结进数据库（前缀缓存要求）。
//!
//! 组装方式：
//! - 使用 Markdown 一级标题分隔各 section
//! - 项目上下文按三层加载，用子标题区分来源
//! - 补充指令每个文件用 `## {完整路径}` 作标题
//!
//! 最终结构：
//! ```text
//! # Agent 定义
//!
//! [agents/default.md 正文，覆盖内置默认]
//!
//! # 项目上下文
//!
//! ## 项目层（最高优先级）
//! [workspace/AGENTS.md 内容]
//!
//! ## Agent 层
//! [Agent 目录的 AGENTS.md 内容]
//!
//! ## 全局层
//! [全局 AGENTS.md 内容]
//!
//! # 工具使用指南
//! [硬编码工具使用原则]
//!
//! # 技能 skills
//! [可用 skills 索引]
//!
//! # 补充指令
//!
//! ## {完整路径1}
//! [instructions/ 文件1 正文]
//!
//! ## {完整路径2}
//! [instructions/ 文件2 正文]
//!
//! # 环境
//!
//! 当前时间：2026-04-22 19:30
//! 运行环境：Windows
//! ```

use crate::sections::{
    build_agent_identity_section, build_datetime_section, build_environment_section,
    build_instructions_section, build_project_context_section, build_skills_section,
    build_subagent_index_section, build_tool_guidance_section,
};
use fuyao_api::{AgentConfig, AgentPaths};

/// 系统提示词的构建用途
///
/// 决定 Agent 身份层的 mode 校验方向，以及子代理索引层是否注入：
/// - [`PromptUsage::Primary`]：主 Agent session（`parent_session_id = None`），
///   校验 `is_usable_as_primary`；注入子代理索引层供 LLM 选子代理
/// - [`PromptUsage::Subagent`]：子代理 session（`parent_session_id = Some`），
///   校验 `is_usable_as_subagent`；不注入子代理索引（子代理不可再派生）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromptUsage {
    /// 主 Agent session
    #[default]
    Primary,
    /// 子代理 session
    Subagent,
}

/// 构建所有 section
///
/// 按顺序调用各层构建函数，返回 (中文标题, 内容) 列表。
/// `usage` 决定身份层校验方向与子代理索引层是否注入。
pub fn build_all_sections(
    agent_paths: &AgentPaths,
    agent_config: &AgentConfig,
    usage: PromptUsage,
) -> Vec<(String, String)> {
    let mut sections: Vec<(String, String)> = Vec::new();

    // Layer 1: Agent 身份（从 agents/{definition}.md 加载，覆盖内置默认；按 usage 校验 mode）
    let content = build_agent_identity_section(agent_paths, agent_config, usage);
    if !content.is_empty() {
        sections.push(("Agent 定义".to_string(), content));
    }

    // Layer 2: 项目上下文（AGENTS.md，已包含多级子标题）
    let content = build_project_context_section(agent_paths);
    if !content.is_empty() {
        sections.push(("项目上下文".to_string(), content));
    }

    // Layer 3: 工具使用引导
    let content = build_tool_guidance_section();
    if !content.is_empty() {
        sections.push(("工具使用指南".to_string(), content));
    }

    // Layer 4: Memory 快照（未来功能）
    // build_memory_section 当前返回空字符串，不加入 sections

    // Layer 5: Skills 索引
    let content = build_skills_section(agent_paths);
    if !content.is_empty() {
        sections.push(("技能 skills".to_string(), content));
    }

    // Layer 5.5: 子代理索引（仅主 Agent 注入：子代理不可再派生，无需此清单）
    if usage == PromptUsage::Primary {
        let content = build_subagent_index_section(agent_paths);
        if !content.is_empty() {
            sections.push(("子代理".to_string(), content));
        }
    }

    // Layer 5.6: 补充指令（instructions/ 文件夹全量拼接）
    let content = build_instructions_section(agent_paths);
    if !content.is_empty() {
        sections.push(("补充指令".to_string(), content));
    }

    // Layer 6+7: 环境（时间 + 运行环境合为一节）
    let datetime_str = build_datetime_section();
    let env_str = build_environment_section();
    let env_content = format!("{datetime_str}\n{env_str}");
    sections.push(("环境".to_string(), env_content));

    sections
}

/// 将 sections 列表组装为完整系统提示词
///
/// 各 section 用 Markdown 一级标题分隔，自然衔接。
fn sections_to_prompt(sections: Vec<(String, String)>) -> String {
    if sections.is_empty() {
        return String::new();
    }

    let parts: Vec<String> = sections
        .iter()
        .map(|(title, content)| format!("# {title}\n\n{content}"))
        .collect();

    parts.join("\n\n")
}

/// 构建系统提示词
///
/// 分层组装各 section，返回完整系统提示词。
/// Layer 1（Agent 身份）从 `agents/{definition}.md` 加载，definition 由 agent_config 提供；
/// 按 `usage` 校验 mode 合法性，并决定是否注入子代理索引层。
pub fn build_system_prompt(
    agent_paths: &AgentPaths,
    agent_config: &AgentConfig,
    usage: PromptUsage,
) -> String {
    sections_to_prompt(build_all_sections(agent_paths, agent_config, usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIMARY: PromptUsage = PromptUsage::Primary;

    #[test]
    fn build_all_sections_returns_non_empty() {
        let ctx = AgentPaths::default();
        let sections = build_all_sections(&ctx, &AgentConfig::default(), PRIMARY);
        assert!(!sections.is_empty());
        // 应该包含 Agent 定义和环境
        let titles: Vec<&str> = sections.iter().map(|(t, _)| t.as_str()).collect();
        assert!(titles.contains(&"Agent 定义"));
        assert!(titles.contains(&"环境"));
    }

    #[test]
    fn build_system_prompt_returns_non_empty() {
        let ctx = AgentPaths::default();
        let prompt = build_system_prompt(&ctx, &AgentConfig::default(), PRIMARY);
        assert!(!prompt.is_empty());
        assert!(prompt.contains("# Agent 定义"));
        assert!(prompt.contains("# 环境"));
        assert!(prompt.contains("Fuyao"));
    }

    #[test]
    fn build_system_prompt_contains_datetime() {
        let ctx = AgentPaths::default();
        let prompt = build_system_prompt(&ctx, &AgentConfig::default(), PRIMARY);
        assert!(prompt.contains("当前时间："));
    }

    #[test]
    fn build_all_sections_order_is_correct() {
        // 默认无 instructions/，补充指令 section 不出现；验证其余顺序
        let ctx = AgentPaths::default();
        let sections = build_all_sections(&ctx, &AgentConfig::default(), PRIMARY);
        let titles: Vec<&str> = sections.iter().map(|(t, _)| t.as_str()).collect();
        // 期望顺序：Agent 定义 → 项目上下文 → 工具使用指南 → 技能 skills → 子代理 → 环境
        let agent_idx = titles.iter().position(|t| *t == "Agent 定义").unwrap();
        let env_idx = titles.iter().position(|t| *t == "环境").unwrap();
        assert!(agent_idx < env_idx);
        // 工具指南在 skills 前
        if let (Some(tool_idx), Some(skills_idx)) = (
            titles.iter().position(|t| *t == "工具使用指南"),
            titles.iter().position(|t| *t == "技能 skills"),
        ) {
            assert!(tool_idx < skills_idx);
            assert!(skills_idx < env_idx);
        }
    }

    #[test]
    fn build_all_sections_includes_instructions_when_present() {
        // 有 instructions/ 时，补充指令应出现在子代理之后、环境之前
        // 通过 extra_dirs 注入，避免环境变量竞争
        let temp = std::env::temp_dir().join("fuyao_test_builder_instructions");
        let plugin = temp.join("plugin");
        let instr_dir = plugin.join("instructions");
        std::fs::create_dir_all(&instr_dir).unwrap();
        std::fs::write(instr_dir.join("rule.md"), "补充规则内容").unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let sections = build_all_sections(&ctx, &AgentConfig::default(), PRIMARY);

        let titles: Vec<&str> = sections.iter().map(|(t, _)| t.as_str()).collect();
        let instr_idx = titles
            .iter()
            .position(|t| *t == "补充指令")
            .expect("补充指令 section 应存在");
        let subagent_idx = titles.iter().position(|t| *t == "子代理");
        let env_idx = titles
            .iter()
            .position(|t| *t == "环境")
            .expect("环境 section 应存在");
        // 补充指令在环境之前
        assert!(instr_idx < env_idx);
        // 若子代理索引存在，补充指令在子代理之后
        if let Some(si) = subagent_idx {
            assert!(si < instr_idx);
        }

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn subagent_usage_omits_subagent_index_layer() {
        // Subagent 用途不注入子代理索引层（子代理不可再派生）
        let ctx = AgentPaths::default();
        let sections = build_all_sections(&ctx, &AgentConfig::default(), PromptUsage::Subagent);
        let titles: Vec<&str> = sections.iter().map(|(t, _)| t.as_str()).collect();
        assert!(!titles.contains(&"子代理"));
    }
}
