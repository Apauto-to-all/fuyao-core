//! Agent 定义查询
//!
//! 定义的列举（按模式过滤的列表查询）与会话定义解析的统一收口出口。
//! 加载链见 [`super::loader`]。

use super::loader::{
    load_agent_definition, load_agent_definition_from_agent_paths, load_builtin_definition,
};
use crate::error::PromptError;
use fuyao_api::{AgentConfig, AgentDefinition, AgentPaths, DefinitionOption};
use std::collections::HashMap;
use std::path::PathBuf;

/// 解析当前 session 使用的完整 Agent 定义
///
/// 统一定义加载 + mode 校验的出口，供两个消费方共享：
/// - 系统提示词构建（builder）：取 `.system_prompt` 拼系统提示词
/// - 引擎装配（`assemble_session`）：持有完整 `AgentDefinition`，用 `.tools` 做
///   per-session 工具可见性过滤（定义层工具配置，与全局 `[tools.enabled]` 取交集）
///
/// 查找链与 [`load_agent_definition_from_agent_paths`] 一致：
/// 用户 `agents/{name}.md` → 内置默认表。定义名由调用方显式提供（`AgentConfig.definition`
/// 必填），链上未命中即 [`PromptError::DefinitionNotFound`]（附按用途过滤的可用列表）；
/// 某层文件存在但损坏即 [`PromptError::DefinitionCorrupted`]（附文件路径与原因）——
/// 不静默跳层或落内置表，绝不静默替换人格。
///
/// mode 校验按 `usage` 方向（主 Agent 校验 `is_usable_as_primary`、子代理校验
/// `is_usable_as_subagent`）；不符返回 [`PromptError::ModeMismatch`]。
pub fn resolve_definition(
    agent_paths: &AgentPaths,
    agent_config: &AgentConfig,
    usage: crate::PromptUsage,
) -> Result<AgentDefinition, PromptError> {
    let def_name = agent_config.definition.as_str();
    let agent_def = load_agent_definition_from_agent_paths(agent_paths, def_name)
        .map_err(|cause| PromptError::DefinitionCorrupted {
            name: def_name.to_string(),
            cause,
        })?
        .ok_or_else(|| {
            // 未知名报错并附按用途过滤的可用列表，供调用方（或依据错误自纠的 LLM）直接修正
            let usable_as = |def: &AgentDefinition| match usage {
                crate::PromptUsage::Primary => def.mode.is_usable_as_primary(),
                crate::PromptUsage::Subagent => def.mode.is_usable_as_subagent(),
            };
            let available: Vec<String> = collect_definitions(agent_paths)
                .into_iter()
                .filter(|opt| usable_as(&opt.definition))
                .map(|opt| opt.id)
                .collect();
            PromptError::DefinitionNotFound {
                name: def_name.to_string(),
                available: available.join("、"),
            }
        })?;
    let usable = match usage {
        crate::PromptUsage::Primary => agent_def.mode.is_usable_as_primary(),
        crate::PromptUsage::Subagent => agent_def.mode.is_usable_as_subagent(),
    };
    if !usable {
        return Err(PromptError::ModeMismatch {
            name: def_name.to_string(),
            mode: agent_def.mode,
            usage,
        });
    }
    Ok(agent_def)
}

/// 收集所有 Agent 定义（四层 .md + 内置，全模式），按优先级去重
///
/// 共享扫描器，供 [`list_primary_definitions`]（过滤主代理模式）、
/// [`list_subagent_definitions`]（过滤子代理模式）与 [`resolve_definition`]
/// 的可用列表生成复用。返回 [`DefinitionOption`] 列表：`id` = file stem
/// （去重键与对外名），`definition` = 解析得到的完整定义。
///
/// 遍历顺序遵循 [`AgentPaths::agents_def_dirs`] 的优先级（workspace > agent > global > extra），
/// 同名定义首现胜（高优先级层覆盖低优先级层）；内置定义（[`crate::builtin::builtin_names`]）
/// 作为最低优先级注入，与用户文件同名时用户文件胜。最后按 id 升序排序。
///
/// 单个定义文件损坏（存在但解析失败）记 WARN 后跳过，不拖垮整个列举——
/// 列举是辅助视图（列表 / 索引 / 报错文案），目标定义的加载路径已由
/// [`resolve_definition`] 独立 fail-loud；此处若因旁支文件损坏而整体失败，
/// 会让「未知名报错」退化为「另一个文件的解析错误」，干扰定位。
fn collect_definitions(agent_paths: &AgentPaths) -> Vec<DefinitionOption> {
    // id(stem) → 定义，首现胜（按优先级顺序插入，已存在则跳过）
    let mut by_id: HashMap<String, AgentDefinition> = HashMap::new();

    let dirs = agent_paths.agents_def_dirs();

    // all() 按优先级排序（workspace > agent > global > extra），read_dir 自身处理目录不存在
    for dir in dirs.all() {
        let mut md_files: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file() && p.extension().is_some_and(|ext| ext == "md"))
                .collect(),
            Err(_) => continue,
        };
        // 目录内按文件名排序，保证同层同优先级下输出稳定
        md_files.sort();

        for file_path in md_files {
            let Some(stem) = file_path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // 首现胜：高优先级层已记录同名则跳过
            if by_id.contains_key(stem) {
                continue;
            }
            match load_agent_definition(&file_path) {
                // 扫描与读取之间文件被删（竞态）→ 视同未命中
                Ok(None) => {}
                Ok(Some(def)) => {
                    by_id.insert(stem.to_string(), def);
                }
                Err(cause) => {
                    tracing::warn!(
                        definition_id = %stem,
                        cause = %cause,
                        "Agent 定义文件损坏，已从列举中跳过"
                    );
                }
            }
        }
    }

    // 内置定义（最低优先级）：与用户文件同名时用户文件胜
    for builtin_name in crate::builtin::builtin_names(crate::builtin::BuiltinKind::Agents) {
        if by_id.contains_key(builtin_name) {
            continue;
        }
        if let Some(def) = load_builtin_definition(builtin_name) {
            by_id.insert(builtin_name.to_string(), def);
        }
    }

    let mut entries: Vec<DefinitionOption> = by_id
        .into_iter()
        .map(|(id, definition)| DefinitionOption { id, definition })
        .collect();
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    entries
}

/// 列举可选主 Agent 定义（人格，仅 Primary 模式）
///
/// 会话人格选择的专用列举：只含可设给 `AgentConfig.definition` 的主代理定义，
/// 过滤口径与 [`resolve_definition`] 的主代理 mode 校验一致——列表里能选中的
/// 定义设为会话人格必然生效。Subagent 模式定义不出现（专职供子代理工具派生，
/// 见 [`list_subagent_definitions`]），两个列表按 mode 互斥。
///
/// 遍历 `agent_paths.agents_def_dirs()` 各层（优先级 workspace > agent > global > extra），
/// 解析每个 `*.md`，按 file stem 作 name，同名按优先级去重（首现胜）。
/// 再注入内置定义为最低优先级（default = Primary 入列；explore / executor =
/// Subagent 被过滤），与用户文件同名时用户文件胜。按 name 排序返回。
pub fn list_primary_definitions(agent_paths: &AgentPaths) -> Vec<DefinitionOption> {
    collect_definitions(agent_paths)
        .into_iter()
        .filter(|opt| opt.definition.mode.is_usable_as_primary())
        .collect()
}

/// 列举可用子代理定义（仅 Subagent 模式）
///
/// 子代理工具的专用列举，供两个消费方共享：
/// - 系统提示词的「子代理」索引层（sections 构建）
/// - 子代理工具 handler：校验 `subagent_type` 是否合法，不合法时返可用列表
///
/// Primary 模式定义不出现（主代理人格不作派生目标），与 [`list_primary_definitions`]
/// 按 mode 互斥。实时查询（每次调用现查，无缓存，会话中途新增 `agents/*.md`
/// 也能立即查到）。`id` 取 file stem（与 [`load_agent_definition_from_agent_paths`]
/// 的 `agents/{name}.md` 查找链一致），用户同名文件覆盖内置。按 id 升序排序返回。
pub fn list_subagent_definitions(agent_paths: &AgentPaths) -> Vec<DefinitionOption> {
    collect_definitions(agent_paths)
        .into_iter()
        .filter(|opt| opt.definition.mode.is_usable_as_subagent())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::AgentMode;

    #[test]
    fn resolve_definition_loads_builtin_default() {
        // definition = "default"（无用户文件）→ 命中内置出厂人格
        let ctx = AgentPaths::default();
        let config = AgentConfig {
            definition: "default".to_string(),
        };
        let def = resolve_definition(&ctx, &config, crate::PromptUsage::Primary).unwrap();
        assert!(!def.system_prompt.is_empty());
        assert!(def.system_prompt.contains("Fuyao"));
    }

    #[test]
    fn resolve_definition_loads_named() {
        // definition = "reviewer" → 加载 agents/reviewer.md
        let temp = std::env::temp_dir().join("fuyao_test_query_def_choice");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        std::fs::write(
            plugin.join("agents").join("reviewer.md"),
            "---\nname: 审查员\n---\n你是代码审查专家",
        )
        .unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let config = AgentConfig {
            definition: "reviewer".to_string(),
        };
        let def = resolve_definition(&ctx, &config, crate::PromptUsage::Primary).unwrap();
        assert!(def.system_prompt.contains("代码审查专家"));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn resolve_definition_unknown_name_reports_available_list() {
        // 未知名报错而非静默换人格；错误信息附按用途过滤的可用列表
        let temp = std::env::temp_dir().join("fuyao_test_query_def_unknown");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        // 主代理与子代理定义各一，验证列表按用途过滤
        std::fs::write(
            plugin.join("agents").join("boss.md"),
            "---\nname: boss\nmode: primary\n---\n主代理",
        )
        .unwrap();
        std::fs::write(
            plugin.join("agents").join("auditor.md"),
            "---\nname: auditor\nmode: subagent\n---\n子代理",
        )
        .unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let config = AgentConfig {
            definition: "defualt".to_string(),
        };
        let err = resolve_definition(&ctx, &config, crate::PromptUsage::Primary).unwrap_err();
        match &err {
            PromptError::DefinitionNotFound { name, available } => {
                assert_eq!(name, "defualt");
                // 主代理用途：boss 与内置 default 在列，Subagent 定义（auditor/explore/executor）被过滤
                assert!(available.contains("boss"));
                assert!(available.contains("default"));
                assert!(!available.contains("auditor"));
                assert!(!available.contains("explore"));
            }
            other => panic!("应为 DefinitionNotFound，实际：{other:?}"),
        }

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn resolve_definition_rejects_subagent_mode_as_primary() {
        // mode: subagent 的定义不能用作主代理 → ModeMismatch 报错
        let temp = std::env::temp_dir().join("fuyao_test_query_subagent_mode");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        std::fs::write(
            plugin.join("agents").join("default.md"),
            "---\nname: sub\ndescription: sub\nmode: subagent\n---\n你是子代理,不应作主代理",
        )
        .unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let config = AgentConfig {
            definition: "default".to_string(),
        };
        let err = resolve_definition(&ctx, &config, crate::PromptUsage::Primary).unwrap_err();
        assert!(matches!(err, PromptError::ModeMismatch { .. }));
        assert!(err.to_string().contains("default"));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn resolve_definition_rejects_primary_mode_as_subagent() {
        // 对称校验：mode: primary 的定义不能用作子代理 → ModeMismatch 报错
        let temp = std::env::temp_dir().join("fuyao_test_query_primary_mode_as_sub");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        std::fs::write(
            plugin.join("agents").join("boss.md"),
            "---\nname: boss\ndescription: 仅主代理\nmode: primary\n---\n你是专属主代理,不应作子代理",
        )
        .unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let config = AgentConfig {
            definition: "boss".to_string(),
        };
        let err = resolve_definition(&ctx, &config, crate::PromptUsage::Subagent).unwrap_err();
        assert!(matches!(err, PromptError::ModeMismatch { .. }));
        assert!(err.to_string().contains("boss"));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn resolve_definition_corrupted_file_reports_error_not_fallback() {
        // 定义文件存在但损坏 → DefinitionCorrupted（含文件路径），
        // 不静默落内置表、不伪装成「未找到」
        let temp = std::env::temp_dir().join("fuyao_test_query_def_corrupted");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        std::fs::write(
            plugin.join("agents").join("default.md"),
            "---\nname: [unclosed\nmode: primary\n---\n坏文件",
        )
        .unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let config = AgentConfig {
            definition: "default".to_string(),
        };
        let err = resolve_definition(&ctx, &config, crate::PromptUsage::Primary).unwrap_err();
        match &err {
            PromptError::DefinitionCorrupted { name, cause } => {
                assert_eq!(name, "default");
                assert!(
                    cause.contains(
                        plugin
                            .join("agents")
                            .join("default.md")
                            .to_string_lossy()
                            .as_ref()
                    ),
                    "错误应含损坏文件路径：{cause}"
                );
            }
            other => panic!("应为 DefinitionCorrupted，实际：{other:?}"),
        }
        // 错误文案透传路径与原因，上层可直接展示
        let msg = err.to_string();
        assert!(msg.contains("文件损坏"), "错误信息应说明损坏：{msg}");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn resolve_definition_carries_tools_field() {
        // resolve_definition 返回完整 AgentDefinition，tools 字段一并带回（供 per-session 过滤）
        let temp = std::env::temp_dir().join("fuyao_test_query_def_tools");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        std::fs::write(
            plugin.join("agents").join("researcher.md"),
            "---\nname: researcher\nmode: subagent\ntools:\n  write: false\n  bash: false\n---\n只读",
        )
        .unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let config = AgentConfig {
            definition: "researcher".to_string(),
        };
        let def = resolve_definition(&ctx, &config, crate::PromptUsage::Subagent).unwrap();
        assert_eq!(def.tools.get("write"), Some(&false));
        assert_eq!(def.tools.get("bash"), Some(&false));
        assert!(!def.tools.contains_key("read"));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn collect_definitions_skips_corrupted_files_and_keeps_rest() {
        // 目录里混有损坏文件时列举不整体失败：坏文件跳过（WARN），好文件保留
        let temp = std::env::temp_dir().join("fuyao_test_query_collect_corrupted");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        std::fs::write(
            plugin.join("agents").join("good.md"),
            "---\nname: good\nmode: primary\n---\n好定义",
        )
        .unwrap();
        std::fs::write(
            plugin.join("agents").join("bad.md"),
            "---\nname: [unclosed\n---\n坏定义",
        )
        .unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let defs = list_primary_definitions(&ctx);
        assert!(defs.iter().any(|d| d.id == "good"), "好定义应保留");
        assert!(
            !defs.iter().any(|d| d.id == "bad"),
            "坏定义应被跳过而非进入列表"
        );

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn list_primary_definitions_primary_only_filters_subagent() {
        // 主代理专用列举：Primary 定义出现，Subagent 定义（用户 helper 与内置
        // explore / executor）被过滤，内置 default（Primary）保留
        let temp = std::env::temp_dir().join("fuyao_test_query_list_primary_only");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        // 一个 primary、一个 subagent
        std::fs::write(
            plugin.join("agents").join("boss.md"),
            "---\nname: boss\ndescription: 专属主代理\nmode: primary\n---\n仅主代理",
        )
        .unwrap();
        std::fs::write(
            plugin.join("agents").join("helper.md"),
            "---\nname: helper\ndescription: 助手\nmode: subagent\n---\n子代理",
        )
        .unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let defs = list_primary_definitions(&ctx);

        let boss = defs.iter().find(|d| d.id == "boss").expect("应含 boss");
        assert_eq!(boss.definition.mode, AgentMode::Primary);
        assert_eq!(boss.definition.description, "专属主代理");
        // Subagent 模式的用户定义与内置定义都不出现
        assert!(!defs.iter().any(|d| d.id == "helper"));
        assert!(!defs.iter().any(|d| d.id == "explore"));
        assert!(!defs.iter().any(|d| d.id == "executor"));
        // 列表里全部是 Primary 模式
        assert!(defs.iter().all(|d| d.definition.mode == AgentMode::Primary));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn list_primary_definitions_includes_builtin_default_only() {
        // 无用户文件时仅注入内置 default（Primary）；explore / executor 为
        // Subagent 模式，不进主代理列举
        let ctx = AgentPaths::default();
        let defs = list_primary_definitions(&ctx);

        let default = defs
            .iter()
            .find(|d| d.id == "default")
            .expect("应注入内置 default");
        assert_eq!(default.definition.mode, AgentMode::Primary);

        assert!(!defs.iter().any(|d| d.id == "explore"));
        assert!(!defs.iter().any(|d| d.id == "executor"));
    }

    #[test]
    fn list_primary_definitions_user_file_overrides_builtin() {
        // 用户 agents/default.md（Primary）覆盖内置 default（同名首现胜，用户文件生效）
        let temp = std::env::temp_dir().join("fuyao_test_query_list_override");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        std::fs::write(
            plugin.join("agents").join("default.md"),
            "---\nname: default\ndescription: 我的人格\nmode: primary\n---\n自定义",
        )
        .unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let defs = list_primary_definitions(&ctx);

        let default = defs
            .iter()
            .find(|d| d.id == "default")
            .expect("应含 default");
        assert_eq!(default.definition.description, "我的人格");
        // 同名去重后只剩一份
        assert_eq!(defs.iter().filter(|d| d.id == "default").count(), 1);

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn list_subagent_definitions_returns_full_options() {
        // 子代理专用列举：返回完整 DefinitionOption（id + definition），
        // 内置 explore / executor 保留，default（Primary）与主代理定义被过滤
        let temp = std::env::temp_dir().join("fuyao_test_query_list_subagent");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        std::fs::write(
            plugin.join("agents").join("boss.md"),
            "---\nname: boss\ndescription: 专属主代理\nmode: primary\n---\n仅主代理",
        )
        .unwrap();
        std::fs::write(
            plugin.join("agents").join("auditor.md"),
            "---\nname: auditor\ndescription: 审计\nmode: subagent\n---\n审计子代理",
        )
        .unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let defs = list_subagent_definitions(&ctx);

        // 用户 subagent 定义与内置 explore / executor 在列
        let auditor = defs
            .iter()
            .find(|d| d.id == "auditor")
            .expect("应含 auditor");
        assert_eq!(auditor.definition.description, "审计");
        assert!(defs.iter().any(|d| d.id == "explore"));
        assert!(defs.iter().any(|d| d.id == "executor"));
        // Primary 模式的（default 与用户 boss）不出现；列表内全部 Subagent 模式
        assert!(!defs.iter().any(|d| d.id == "default"));
        assert!(!defs.iter().any(|d| d.id == "boss"));
        assert!(
            defs.iter()
                .all(|d| d.definition.mode == AgentMode::Subagent)
        );

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn list_primary_definitions_sorted_by_name() {
        // 输出按 name 升序排序
        let temp = std::env::temp_dir().join("fuyao_test_query_list_sorted");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        std::fs::write(
            plugin.join("agents").join("zebra.md"),
            "---\nname: zebra\nmode: primary\n---\nz",
        )
        .unwrap();
        std::fs::write(
            plugin.join("agents").join("alpha.md"),
            "---\nname: alpha\nmode: primary\n---\na",
        )
        .unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let defs = list_primary_definitions(&ctx);
        let names: Vec<&str> = defs.iter().map(|d| d.id.as_str()).collect();
        // 升序：alpha 在 zebra 前
        assert!(names.windows(2).all(|w| w[0] <= w[1]));
        assert!(
            names.iter().position(|n| *n == "alpha") < names.iter().position(|n| *n == "zebra")
        );

        std::fs::remove_dir_all(&temp).ok();
    }
}
