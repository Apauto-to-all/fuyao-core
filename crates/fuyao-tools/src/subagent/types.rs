//! 子代理工具类型定义
//!
//! 定义子代理校验与错误格式化的辅助类型。
//!
//! 子代理工具与 skill 工具同源：调用时实时查询可用列表（无缓存），
//! `subagent_type` 校验失败时返回带可用列表的错误字符串，供 LLM 据此修正。

use fuyao_api::AgentPaths;

/// 子代理元数据（用于校验失败时的可用列表显示）
#[derive(Debug, Clone)]
pub struct SubagentMetaItem {
    /// 子代理定义名（`subagent_type` 取值）
    pub name: String,
    /// 子代理描述
    pub description: String,
}

/// 列出可用子代理定义
///
/// 实时查询 `fuyao_prompt::list_subagent_definitions`：扫 `agents/` 目录 + 内置默认，
/// 过滤 `is_usable_as_subagent`。与系统提示词「子代理」索引层同源，每次调用现查
/// （无缓存，反映最新状态——用户会话中途新增 `agents/*.md` 也能立即查到）。
pub fn list_available_subagents(agent_paths: &AgentPaths) -> Vec<SubagentMetaItem> {
    fuyao_prompt::list_subagent_definitions(agent_paths)
        .into_iter()
        .map(|(name, description)| SubagentMetaItem { name, description })
        .collect()
}

/// 校验 `subagent_type` 是否在可用列表中
///
/// - 在列表中 → `Ok(())`
/// - 不在列表中 → `Err(带可用列表的错误字符串)`，handler 直接返给 LLM
///
/// 同时挡住两类错误：① 拼错或不存在的名字；② `mode: primary` 的定义（专属主 Agent，
/// 不应派生为子代理）。后者不会进可用列表（已按 `is_usable_as_subagent` 过滤）。
pub fn validate_subagent_type(subagent_type: &str, agent_paths: &AgentPaths) -> Result<(), String> {
    let available = list_available_subagents(agent_paths);
    if available.iter().any(|m| m.name == subagent_type) {
        Ok(())
    } else {
        Err(format_subagent_not_found(subagent_type, &available))
    }
}

/// 格式化 `subagent_type` 未找到错误
///
/// 错误信息 + 可用列表（name + description），供 LLM 据此修正 `subagent_type`。
/// 镜像 skill 工具的 `format_skill_not_found`，但额外带上描述——子代理列表通常很短
/// （内置 researcher/executor + 少量用户定义），带描述更利于 LLM 选对子代理。
fn format_subagent_not_found(name: &str, available: &[SubagentMetaItem]) -> String {
    if available.is_empty() {
        format!("未找到子代理类型 '{name}'。当前没有可用的子代理")
    } else {
        let entries: Vec<String> = available
            .iter()
            .map(|m| {
                if m.description.is_empty() {
                    m.name.clone()
                } else {
                    format!("{}（{}）", m.name, m.description)
                }
            })
            .collect();
        format!(
            "未找到子代理类型 '{name}'。可用的子代理: {}",
            entries.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_builtin_researcher() {
        let paths = AgentPaths::default();
        // researcher 是内置子代理，永远可用
        assert!(validate_subagent_type("researcher", &paths).is_ok());
    }

    #[test]
    fn validate_accepts_builtin_executor() {
        let paths = AgentPaths::default();
        assert!(validate_subagent_type("executor", &paths).is_ok());
    }

    #[test]
    fn validate_rejects_unknown_type_with_available_list() {
        let paths = AgentPaths::default();
        let result = validate_subagent_type("nonexistent", &paths);
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("未找到子代理类型 'nonexistent'"));
        // 错误信息含可用列表（含描述），引导 LLM 修正
        assert!(msg.contains("researcher"));
        assert!(msg.contains("executor"));
        assert!(msg.contains("只读探索"));
    }

    #[test]
    fn validate_rejects_primary_mode_def() {
        // mode: primary 的定义不在可用子代理列表中 → 被拒
        let temp = std::env::temp_dir().join("fuyao_test_subagent_validate_primary");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        std::fs::write(
            plugin.join("agents").join("boss.md"),
            "---\nname: boss\ndescription: 专属主代理\nmode: primary\n---\n仅主代理",
        )
        .unwrap();

        let paths = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let result = validate_subagent_type("boss", &paths);
        assert!(result.is_err(), "mode:primary 的定义不应作为子代理");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn validate_accepts_user_subagent_def() {
        // 用户自定义 mode:subagent 定义应通过校验
        let temp = std::env::temp_dir().join("fuyao_test_subagent_validate_user");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        std::fs::write(
            plugin.join("agents").join("auditor.md"),
            "---\nname: auditor\ndescription: 审计\nmode: subagent\n---\n审计子代理",
        )
        .unwrap();

        let paths = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        assert!(validate_subagent_type("auditor", &paths).is_ok());

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn format_not_found_empty_list() {
        let msg = format_subagent_not_found("foo", &[]);
        assert!(msg.contains("当前没有可用的子代理"));
    }
}
