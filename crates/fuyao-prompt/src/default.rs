//! 默认 Agent 定义（内置，编译期嵌入）
//!
//! 框架内置的默认 Agent 定义，硬编码进二进制确保框架开箱即用。
//! 用户可通过 `agents/{name}.md` 覆盖同名内置定义（加载链见 [`crate::loader`]）。
//!
//! 文件组织：默认提示词按用途分放在 `defaults/{primary,subagent}/*.md`，
//! 通过 [`include_str!`] 在编译期整体嵌入二进制，运行时零读盘、零资产依赖。
//! 文件本身仍是可语法高亮的 Markdown，修改后重新编译即生效。

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
    &[fuyao_api::DEFAULT_DEFINITION_NAME, "explore", "executor"]
}

/// 按 name 取内置默认定义的原始 Markdown 文本
///
/// 覆盖链的一环：用户 `agents/{name}.md` 不存在时查本表。仅精确匹配内置名
/// （空串不映射到任何内置定义），未命中返回 `None`，由调用方决定错误语义。
pub(crate) fn builtin_definition_md(name: &str) -> Option<&'static str> {
    match name {
        fuyao_api::DEFAULT_DEFINITION_NAME => Some(PRIMARY_DEFAULT_MD),
        "explore" => Some(SUBAGENT_EXPLORE_MD),
        "executor" => Some(SUBAGENT_EXECUTOR_MD),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_definition_md_known_names() {
        assert!(builtin_definition_md("default").is_some());
        assert!(builtin_definition_md("explore").is_some());
        assert!(builtin_definition_md("executor").is_some());
    }

    #[test]
    fn builtin_definition_md_rejects_empty_name() {
        // 空串不是合法内置名：显式人格语义下未知名（含空串）一律走报错路径
        assert!(builtin_definition_md("").is_none());
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
