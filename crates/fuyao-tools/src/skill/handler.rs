//! Skills 工具处理函数
//!
//! 加载并浏览 Skills。
//! - 不传参数 → 列出所有 Skills
//! - 传 name → 加载 Skill 内容
//! - 传 name + file_path → 加载关联文件

use super::types::{SkillFileResult, SkillListResult, SkillMetaItem, SkillViewResult};
use crate::common;
use fuyao_api::{AgentPaths, ToolCallContext};
use fuyao_skills::{find_all_skills, load_skill, load_skill_file};
use serde_json::Value;

/// 格式化 Skill 未找到错误
fn format_skill_not_found(name: &str, agent_paths: &AgentPaths) -> String {
    let available = find_all_skills(agent_paths).unwrap_or_default();
    if available.is_empty() {
        format!("未找到 Skill '{}'。当前没有可用的 Skills", name)
    } else {
        format!(
            "未找到 Skill '{}'。可用的 Skills: {}",
            name,
            available
                .iter()
                .take(20)
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// 格式化文件未找到错误
fn format_file_not_found(name: &str, file_path: &str, agent_paths: &AgentPaths) -> String {
    let hint = match load_skill(name, agent_paths) {
        Ok(skill) => {
            if skill.linked_files.is_empty() {
                format!("Skill '{}' 没有关联文件", name)
            } else {
                let files: Vec<String> = skill.linked_files.values().flatten().cloned().collect();
                format!("可用的文件: {}", files.join(", "))
            }
        }
        Err(_) => format_skill_not_found(name, agent_paths),
    };
    format!("未找到 Skill '{}' 中的文件 '{}'。{}", name, file_path, hint)
}

/// 列出所有可用 Skills
fn skill_list_handler(ctx: &ToolCallContext) -> String {
    let agent_paths = match &ctx.agent_paths {
        Some(paths) => paths,
        None => return common::tool_error("无法获取 Agent 路径上下文"),
    };

    let all_skills = match find_all_skills(agent_paths) {
        Ok(skills) => skills,
        Err(e) => return common::tool_error(&format!("加载 Skills 失败: {e}")),
    };

    if all_skills.is_empty() {
        let result = SkillListResult {
            skills: vec![],
            count: 0,
            message: Some("未找到 Skills".to_string()),
            hint: None,
        };
        return serde_json::to_string(&result).unwrap_or_else(|_| common::tool_error("序列化失败"));
    }

    let skills: Vec<SkillMetaItem> = all_skills.iter().map(SkillMetaItem::from).collect();

    let count = skills.len();
    let result = SkillListResult {
        skills,
        count,
        message: None,
        hint: Some("使用 skill(name) 查看完整内容".to_string()),
    };

    serde_json::to_string(&result).unwrap_or_else(|_| common::tool_error("序列化失败"))
}

/// 加载 Skill 完整内容或关联文件
fn skill_view_handler(name: &str, file_path: Option<&str>, ctx: &ToolCallContext) -> String {
    let agent_paths = match &ctx.agent_paths {
        Some(paths) => paths,
        None => return common::tool_error("无法获取 Agent 路径上下文"),
    };

    // 如果指定了 file_path，加载关联文件
    if let Some(file_path) = file_path {
        match load_skill_file(name, file_path, agent_paths) {
            Ok(content) => {
                let result = SkillFileResult { content };
                serde_json::to_string(&result).unwrap_or_else(|_| common::tool_error("序列化失败"))
            }
            Err(_) => common::tool_error(&format_file_not_found(name, file_path, agent_paths)),
        }
    } else {
        // 加载 Skill 完整内容
        match load_skill(name, agent_paths) {
            Ok(skill) => {
                let linked_files = if skill.linked_files.is_empty() {
                    None
                } else {
                    Some(skill.linked_files.clone())
                };

                let usage_hint = if skill.linked_files.is_empty() {
                    None
                } else {
                    Some(
                        "使用 skill(name, file_path) 查看关联文件，file_path 如 'references/api.md'"
                            .to_string(),
                    )
                };

                let metadata = if skill.metadata.is_empty() {
                    None
                } else {
                    Some(skill.metadata.clone())
                };

                let result = SkillViewResult {
                    name: skill.name,
                    description: skill.description,
                    content: skill.body,
                    license: skill.license,
                    compatibility: skill.compatibility,
                    metadata,
                    linked_files,
                    usage_hint,
                    skill_dir: skill.skill_dir,
                };
                serde_json::to_string(&result).unwrap_or_else(|_| common::tool_error("序列化失败"))
            }
            Err(_) => common::tool_error(&format_skill_not_found(name, agent_paths)),
        }
    }
}

/// skill 工具处理函数
///
/// 不传参数 → 列出所有 Skills
/// 传 name → 加载 Skill 内容
/// 传 name + file_path → 加载关联文件
pub fn skill_handler(args: Value, ctx: &ToolCallContext) -> String {
    let name = args.get("name").and_then(|v| v.as_str());
    let file_path = args
        .get("file_path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty());

    // file_path 存在但 name 缺失，报错
    if file_path.is_some() && (name.is_none() || name.unwrap().trim().is_empty()) {
        return common::tool_error("缺少必需参数 'name'");
    }

    match name {
        None | Some("") => skill_list_handler(ctx),
        Some(name) => skill_view_handler(name.trim(), file_path, ctx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::AgentPaths;

    fn create_test_ctx() -> ToolCallContext {
        ToolCallContext {
            session_id: Some("test_session".to_string()),
            agent_paths: Some(AgentPaths::default()),
            subagent_ops: None,
        }
    }

    #[test]
    fn skill_handler_empty_name_lists_skills() {
        let args = serde_json::json!({});
        let ctx = create_test_ctx();
        let result = skill_handler(args, &ctx);
        assert!(result.contains("count"));
    }

    #[test]
    fn skill_handler_nonexistent_skill_returns_error() {
        let args = serde_json::json!({ "name": "nonexistent_skill_12345" });
        let ctx = create_test_ctx();
        let result = skill_handler(args, &ctx);
        assert!(result.contains("error"));
        assert!(result.contains("未找到 Skill"));
    }

    #[test]
    fn skill_handler_empty_name_param_lists_skills() {
        let args = serde_json::json!({ "name": "" });
        let ctx = create_test_ctx();
        let result = skill_handler(args, &ctx);
        assert!(result.contains("count"));
    }

    #[test]
    fn skill_handler_nonexistent_file_returns_error() {
        let args = serde_json::json!({
            "name": "nonexistent_skill_12345",
            "file_path": "nonexistent/file.txt"
        });
        let ctx = create_test_ctx();
        let result = skill_handler(args, &ctx);
        assert!(result.contains("error"));
    }

    #[test]
    fn skill_handler_no_agent_paths_returns_error() {
        let args = serde_json::json!({});
        let ctx = ToolCallContext::default();
        let result = skill_handler(args, &ctx);
        assert!(result.contains("error"));
        assert!(result.contains("无法获取 Agent 路径上下文"));
    }
}
