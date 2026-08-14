//! 默认 Agent 定义（内置，编译期嵌入）
//!
//! 框架内置的默认 Agent 定义，硬编码进二进制确保框架开箱即用。
//! 用户可通过 `agents/{name}.md` 覆盖同名内置定义（加载链见 [`crate::loader`]）。
//!
//! 文件组织：默认提示词按用途分放在 `defaults/{primary,subagent}/*.md`，
//! 通过 [`include_str!`] 在编译期整体嵌入二进制，运行时零读盘、零资产依赖。
//! 文件本身仍是可语法高亮的 Markdown，修改后重新编译即生效。

use fuyao_api::AgentDefinition;
use std::sync::LazyLock;

/// 内置主 Agent 默认定义（`defaults/primary/default.md`）
const PRIMARY_DEFAULT_MD: &str = include_str!("defaults/primary/default.md");

/// 内置探索子代理默认定义（`defaults/subagent/explore.md`）
const SUBAGENT_EXPLORE_MD: &str = include_str!("defaults/subagent/explore.md");

/// 内置执行子代理默认定义（`defaults/subagent/executor.md`）
const SUBAGENT_EXECUTOR_MD: &str = include_str!("defaults/subagent/executor.md");

/// 内置定义名全集（`default` / `explore` / `executor`）
///
/// 「遍历所有内置名」的调用方（定义列举的最低优先级注入等）一律取本清单，
/// 禁止另写硬编码列表。清单与 [`builtin_definition_md`] 的 match 表由测试
/// `builtin_definition_names_match_lookup_table` 保证一致。
pub(crate) fn builtin_definition_names() -> &'static [&'static str] {
    &["default", "explore", "executor"]
}

/// 按 name 取内置默认定义的原始 Markdown 文本
///
/// 覆盖链的最后一环：用户 `agents/{name}.md` 不存在时，先查内置默认；
/// 内置也没有该 name 时，调用方再回退到 [`DEFAULT_FUYAO_AGENT`]。
///
/// `""` 与 `"default"` 都映射到主 Agent 默认定义（与 `AgentConfig.definition = None`
/// 时加载 `"default"` 的约定一致）。
pub(crate) fn builtin_definition_md(name: &str) -> Option<&'static str> {
    match name {
        "" | "default" => Some(PRIMARY_DEFAULT_MD),
        "explore" => Some(SUBAGENT_EXPLORE_MD),
        "executor" => Some(SUBAGENT_EXECUTOR_MD),
        _ => None,
    }
}

/// 默认主 Agent 定义（全局单例）
///
/// 由 [`PRIMARY_DEFAULT_MD`] 解析而来，保持「文件即唯一来源」——
/// 改 `defaults/primary/default.md` 即改默认 Agent，无需同步两处。
/// 解析失败（内置文件格式错误）直接 panic：编译期嵌入内容受开发者完全掌控，
/// 解析失败属开发期 bug，应尽早暴露而非静默回退。
pub static DEFAULT_FUYAO_AGENT: LazyLock<AgentDefinition> = LazyLock::new(|| {
    crate::loader::parse_definition_from_content(PRIMARY_DEFAULT_MD, None)
        .expect("内置默认 Agent 定义解析失败：defaults/primary/default.md 格式错误")
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_agent_has_correct_name() {
        assert_eq!(DEFAULT_FUYAO_AGENT.name, "fuyao");
    }

    #[test]
    fn default_agent_has_system_prompt() {
        assert!(!DEFAULT_FUYAO_AGENT.system_prompt.is_empty());
        assert!(DEFAULT_FUYAO_AGENT.system_prompt.contains("Fuyao"));
    }

    #[test]
    fn default_agent_is_cloneable() {
        let clone = DEFAULT_FUYAO_AGENT.clone();
        assert_eq!(clone.name, DEFAULT_FUYAO_AGENT.name);
    }

    #[test]
    fn builtin_definition_md_known_names() {
        assert!(builtin_definition_md("default").is_some());
        assert!(builtin_definition_md("").is_some());
        assert!(builtin_definition_md("explore").is_some());
        assert!(builtin_definition_md("executor").is_some());
    }

    #[test]
    fn builtin_definition_md_unknown_name() {
        assert!(builtin_definition_md("nonexistent").is_none());
    }

    /// 内置名清单与 match 表一致性：清单里的每个名字都能查到内置 Markdown，
    /// 新增内置定义时两处必须同步，否则本测试失败。
    #[test]
    fn builtin_definition_names_match_lookup_table() {
        for name in builtin_definition_names() {
            assert!(
                builtin_definition_md(name).is_some(),
                "清单中的名字 {name} 应能查到内置定义"
            );
        }
    }
}
