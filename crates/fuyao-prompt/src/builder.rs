//! 系统提示词组装器
//!
//! 负责分层组装系统提示词。
//!
//! 系统提示词由 Agent 模块管理，每次对话动态构建。
//! 各层 section 根据当前上下文实时生成。
//!
//! 组装方式：
//! - 使用 Markdown 标题分隔各模块
//! - 项目上下文按三层加载，用子标题区分来源
//!
//! 最终结构：
//! ```text
//! # Agent 定义
//!
//! [Agent 系统提示词]
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
//! # 环境
//!
//! 当前时间：2026-04-22 19:30
//! 运行环境：Windows
//! ```

use crate::sections::{
    build_agent_identity_section, build_datetime_section, build_environment_section,
    build_project_context_section, build_skills_section, build_tool_guidance_section,
};
use fuyao_api::AgentPaths;
use fuyao_api::prompt_types::AgentDefinition;

/// 构建所有 section
///
/// 按顺序调用各层构建函数，返回 (中文标题, 内容) 列表。
///
/// `identity` 不为 None 时覆盖 Layer 1（Agent 身份），其余 6 层不变。
pub fn build_all_sections(
    agent_paths: &AgentPaths,
    identity: Option<&AgentDefinition>,
) -> Vec<(String, String)> {
    let mut sections: Vec<(String, String)> = Vec::new();

    // Layer 1: Agent 身份（identity 覆盖时用 identity，否则从 agent_paths 加载）
    let content = match identity {
        Some(def) => def.system_prompt.clone(),
        None => build_agent_identity_section(agent_paths),
    };
    if !content.is_empty() {
        sections.push(("Agent 定义".to_string(), content));
    }

    // Layer 2: 项目上下文（已包含多级子标题）
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
/// Layer 1（Agent 身份）从 agent_paths 加载 system.md。
pub fn build_system_prompt(agent_paths: &AgentPaths) -> String {
    sections_to_prompt(build_all_sections(agent_paths, None))
}

/// 构建系统提示词（带身份覆盖）
///
/// 与 build_system_prompt 相同，但 Layer 1 用传入的 identity 替代 agent_paths 加载。
/// 用于 Master 等特殊角色复用 Layer 2-7（项目上下文/工具指南/环境），仅替换身份层。
pub fn build_system_prompt_with_identity(
    agent_paths: &AgentPaths,
    identity: &AgentDefinition,
) -> String {
    sections_to_prompt(build_all_sections(agent_paths, Some(identity)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_all_sections_returns_non_empty() {
        let ctx = AgentPaths::default();
        let sections = build_all_sections(&ctx, None);
        assert!(!sections.is_empty());
        // 应该包含 Agent 定义和环境
        let titles: Vec<&str> = sections.iter().map(|(t, _)| t.as_str()).collect();
        assert!(titles.contains(&"Agent 定义"));
        assert!(titles.contains(&"环境"));
    }

    #[test]
    fn build_system_prompt_returns_non_empty() {
        let ctx = AgentPaths::default();
        let prompt = build_system_prompt(&ctx);
        assert!(!prompt.is_empty());
        assert!(prompt.contains("# Agent 定义"));
        assert!(prompt.contains("# 环境"));
        assert!(prompt.contains("Fuyao"));
    }

    #[test]
    fn build_system_prompt_contains_datetime() {
        let ctx = AgentPaths::default();
        let prompt = build_system_prompt(&ctx);
        assert!(prompt.contains("当前时间："));
    }

    #[test]
    fn build_system_prompt_with_identity_replaces_layer1() {
        let ctx = AgentPaths::default();
        let identity = AgentDefinition::new("master", "设计师", "我是工作流设计师");
        let prompt = build_system_prompt_with_identity(&ctx, &identity);
        // Layer 1 应为 identity 的 system_prompt
        assert!(prompt.contains("我是工作流设计师"));
        // Layer 2-7 仍然存在（环境层）
        assert!(prompt.contains("# 环境"));
        assert!(prompt.contains("当前时间："));
    }

    #[test]
    fn build_system_prompt_without_identity_uses_default() {
        let ctx = AgentPaths::default();
        let prompt = build_system_prompt(&ctx);
        // 无 identity_override → 走默认 Agent 身份
        assert!(prompt.contains("Fuyao"));
    }
}
