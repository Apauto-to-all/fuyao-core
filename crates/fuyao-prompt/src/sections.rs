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

use crate::default::DEFAULT_FUYAO_AGENT;
use crate::loader::{
    load_agent_definition, load_agent_definition_from_agent_paths, load_builtin_definition,
};
use chrono::Local;
use fuyao_api::{AgentConfig, AgentDefinition, AgentPaths, DefinitionOption, Source};
use fuyao_skills::find_all_skills;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 解析当前 session 使用的完整 Agent 定义
///
/// 统一定义加载 + mode 校验 + 回退的出口，供两个消费方共享：
/// - [`build_agent_identity_section`]：取 `.system_prompt` 拼系统提示词
/// - 引擎装配（`assemble_session`）：持有完整 `AgentDefinition`，用 `.tools` 做
///   per-session 工具可见性过滤（定义层工具配置，与全局 `[tools.enabled]` 取交集）
///
/// 加载链与 [`load_agent_definition_from_agent_paths`] 一致：
/// 用户 `agents/{name}.md` → 内置默认 → [`DEFAULT_FUYAO_AGENT`]。
/// `agent_config.definition` 为 None 时加载 `"default"`。
///
/// mode 校验按 `usage` 方向（主 Agent 校验 `is_usable_as_primary`、子代理校验
/// `is_usable_as_subagent`）；不合法回退完整 [`DEFAULT_FUYAO_AGENT`]——此时
/// `tools` 也回退为默认（空 = 无限制）。
pub fn resolve_definition(
    agent_paths: &AgentPaths,
    agent_config: &AgentConfig,
    usage: crate::PromptUsage,
) -> AgentDefinition {
    let def_name = agent_config.definition.as_deref().unwrap_or("default");
    let agent_def = load_agent_definition_from_agent_paths(agent_paths, def_name);
    let usable = match usage {
        crate::PromptUsage::Primary => agent_def.mode.is_usable_as_primary(),
        crate::PromptUsage::Subagent => agent_def.mode.is_usable_as_subagent(),
    };
    if !usable {
        tracing::error!(
            definition = def_name,
            mode = ?agent_def.mode,
            ?usage,
            "Agent 定义的 mode 与当前用途不符，回退默认主 Agent 定义"
        );
        return DEFAULT_FUYAO_AGENT.clone();
    }
    agent_def
}

/// 构建 Agent 身份 section（Layer 1）
///
/// `definition` 已由调用方经 [`resolve_definition`] 加载（含 mode 校验 + 回退），
/// 本函数仅取其 `system_prompt`。mode 校验 / 加载 / 回退逻辑统一收口于 [`resolve_definition`]。
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
// TODO: 当前为 stub，Memory 系统实现后启用
#[allow(dead_code)]
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

/// 收集所有 Agent 定义（四层 .md + 内置），按优先级去重
///
/// 共享扫描器，供 [`list_definitions`]（全模式）与 [`list_subagent_definitions`]（过滤
/// 子代理模式）复用。返回 [`DefinitionOption`] 列表：`id` = file stem（去重键与对外名），
/// `definition` = 解析得到的完整定义，`source` = 来源层。
///
/// 遍历顺序遵循 [`AgentPaths::agents_def_dirs`] 的优先级（workspace > agent > global > extra），
/// 同名定义首现胜（高优先级层覆盖低优先级层）；内置定义（default / explore / executor）
/// 作为最低优先级注入，与用户文件同名时用户文件胜。最后按 id 升序排序。
fn collect_definitions(agent_paths: &AgentPaths) -> Vec<DefinitionOption> {
    // id(stem) → (定义, 来源)，首现胜（按优先级顺序插入，已存在则跳过）
    let mut by_id: HashMap<String, (AgentDefinition, Source)> = HashMap::new();

    let dirs = agent_paths.agents_def_dirs();

    // 按优先级构造 (目录, 来源) 列表（read_dir 自身处理目录不存在）
    let mut layered: Vec<(&Path, Source)> = Vec::new();
    if let Some(d) = dirs.workspace.as_deref() {
        layered.push((d, Source::Workspace));
    }
    if let Some(d) = dirs.agent.as_deref() {
        layered.push((d, Source::Agent));
    }
    if let Some(d) = dirs.global_.as_deref() {
        layered.push((d, Source::Global));
    }
    for d in &dirs.extra {
        layered.push((d.as_path(), Source::Extra));
    }

    for (dir, source) in layered {
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
            if let Some(def) = load_agent_definition(&file_path) {
                by_id.insert(stem.to_string(), (def, source));
            }
        }
    }

    // 内置定义（最低优先级）：与用户文件同名时用户文件胜
    for builtin_name in ["default", "explore", "executor"] {
        if by_id.contains_key(builtin_name) {
            continue;
        }
        if let Some(def) = load_builtin_definition(builtin_name) {
            by_id.insert(builtin_name.to_string(), (def, Source::Builtin));
        }
    }

    let mut entries: Vec<DefinitionOption> = by_id
        .into_iter()
        .map(|(id, (definition, source))| DefinitionOption {
            id,
            source,
            definition,
        })
        .collect();
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    entries
}

/// 列举所有可用 Agent 定义（四层 .md + 内置），全模式（Primary + Subagent）
///
/// 遍历 `agent_paths.agents_def_dirs()` 各层（优先级 workspace > agent > global > extra），
/// 解析每个 `*.md`，按 file stem 作 name，同名按优先级去重（首现胜）。
/// 再注入内置定义（default = Primary、explore / executor = Subagent）为最低优先级，
/// 与用户文件同名时用户文件胜。按 name 排序返回。
pub fn list_definitions(agent_paths: &AgentPaths) -> Vec<DefinitionOption> {
    collect_definitions(agent_paths)
}

/// 收集可用子代理定义（name + description）
///
/// 实时查询（每次调用现查，无缓存），供两个消费方共享：
/// - [`build_subagent_index_section`]：格式化为系统提示词的「子代理」索引层
/// - 子代理工具 handler：校验 `subagent_type` 是否合法，不合法时返可用列表
///
/// 基于 [`collect_definitions`] 全量扫描后过滤 `is_usable_as_subagent()`：内置主 Agent
///（default = Primary）被过滤掉，explore / executor（Subagent）保留。
/// name 取 file stem（与 [`load_agent_definition_from_agent_paths`] 的 `agents/{name}.md`
/// 查找链一致），用户同名文件覆盖内置。返回结果按 name 升序排序，输出稳定可读。
pub fn list_subagent_definitions(agent_paths: &AgentPaths) -> Vec<(String, String)> {
    collect_definitions(agent_paths)
        .into_iter()
        .filter(|opt| opt.definition.mode.is_usable_as_subagent())
        .map(|opt| (opt.id, opt.definition.description))
        .collect()
}

/// 构建子代理索引 section（Layer 5.5）
///
/// 列出可用的子代理定义（name + description），供主 Agent 通过 `subagent` 工具的
/// `subagent_type` 参数选择。仅注入主 Agent session（子代理不可再派生）。
/// 数据来自 [`list_subagent_definitions`]，空列表返回空字符串（section 被跳过）。
pub fn build_subagent_index_section(agent_paths: &AgentPaths) -> String {
    let entries = list_subagent_definitions(agent_paths);

    if entries.is_empty() {
        return String::new();
    }

    let mut lines = vec!["可用子代理（subagent 工具的 subagent_type 参数可选值）：".to_string()];
    for (name, desc) in &entries {
        let desc = if desc.is_empty() {
            String::new()
        } else {
            format!("：{desc}")
        };
        lines.push(format!("- {name}{desc}"));
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
    use fuyao_api::AgentMode;

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
    fn resolve_definition_returns_default() {
        let ctx = AgentPaths::default();
        let def = resolve_definition(&ctx, &AgentConfig::default(), crate::PromptUsage::Primary);
        assert!(!def.system_prompt.is_empty());
        assert!(def.system_prompt.contains("Fuyao"));
    }

    #[test]
    fn resolve_definition_loads_named() {
        // definition = Some("reviewer") → 加载 agents/reviewer.md
        let temp = std::env::temp_dir().join("fuyao_test_sections_def_choice");
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
            definition: Some("reviewer".to_string()),
        };
        let def = resolve_definition(&ctx, &config, crate::PromptUsage::Primary);
        assert!(def.system_prompt.contains("代码审查专家"));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn resolve_definition_rejects_subagent_mode_as_primary() {
        // mode: subagent 的定义不能用作主代理，应回退完整 DEFAULT_FUYAO_AGENT
        let temp = std::env::temp_dir().join("fuyao_test_sections_subagent_mode");
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
        let def = resolve_definition(&ctx, &AgentConfig::default(), crate::PromptUsage::Primary);
        // subagent 被拒,回退默认(含 "Fuyao"),不含子代理提示词
        assert!(def.system_prompt.contains("Fuyao"));
        assert!(!def.system_prompt.contains("不应作主代理"));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn resolve_definition_rejects_primary_mode_as_subagent() {
        // 对称校验：mode: primary 的定义不能用作子代理，应回退完整 DEFAULT_FUYAO_AGENT
        let temp = std::env::temp_dir().join("fuyao_test_sections_primary_mode_as_sub");
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
            definition: Some("boss".to_string()),
        };
        let def = resolve_definition(&ctx, &config, crate::PromptUsage::Subagent);
        // primary 被拒,回退默认(含 "Fuyao"),不含 primary 专属提示词
        assert!(def.system_prompt.contains("Fuyao"));
        assert!(!def.system_prompt.contains("不应作子代理"));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn resolve_definition_carries_tools_field() {
        // resolve_definition 返回完整 AgentDefinition，tools 字段一并带回（供 per-session 过滤）
        let temp = std::env::temp_dir().join("fuyao_test_sections_def_tools");
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
            definition: Some("researcher".to_string()),
        };
        let def = resolve_definition(&ctx, &config, crate::PromptUsage::Subagent);
        assert_eq!(def.tools.get("write"), Some(&false));
        assert_eq!(def.tools.get("bash"), Some(&false));
        assert!(!def.tools.contains_key("read"));

        std::fs::remove_dir_all(&temp).ok();
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
    fn list_definitions_lists_all_modes() {
        // 全模式列举：Primary 与 Subagent 定义都出现
        let temp = std::env::temp_dir().join("fuyao_test_list_defs_all_modes");
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
        let defs = list_definitions(&ctx);

        let boss = defs.iter().find(|d| d.id == "boss").expect("应含 boss");
        assert_eq!(boss.definition.mode, AgentMode::Primary);
        assert_eq!(boss.definition.description, "专属主代理");
        let helper = defs.iter().find(|d| d.id == "helper").expect("应含 helper");
        assert_eq!(helper.definition.mode, AgentMode::Subagent);

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn list_definitions_includes_builtins_when_no_user_files() {
        // 无用户文件时注入内置 default(Primary) / explore / executor(Subagent)
        let ctx = AgentPaths::default();
        let defs = list_definitions(&ctx);

        let default = defs
            .iter()
            .find(|d| d.id == "default")
            .expect("应注入内置 default");
        assert_eq!(default.definition.mode, AgentMode::Primary);
        assert_eq!(default.source, Source::Builtin);

        let explore = defs
            .iter()
            .find(|d| d.id == "explore")
            .expect("应注入内置 explore");
        assert_eq!(explore.definition.mode, AgentMode::Subagent);
        assert_eq!(explore.source, Source::Builtin);

        let executor = defs
            .iter()
            .find(|d| d.id == "executor")
            .expect("应注入内置 executor");
        assert_eq!(executor.definition.mode, AgentMode::Subagent);
        assert_eq!(executor.source, Source::Builtin);
    }

    #[test]
    fn list_definitions_user_file_overrides_builtin() {
        // 用户 agents/explore.md 覆盖内置 explore（同名首现胜，source 跟随用户层）
        let temp = std::env::temp_dir().join("fuyao_test_list_defs_override");
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
        let defs = list_definitions(&ctx);

        let explore = defs
            .iter()
            .find(|d| d.id == "explore")
            .expect("应含 explore");
        assert_eq!(explore.definition.description, "我的自定义探索");
        assert_eq!(explore.source, Source::Extra);
        // 内置 explore 描述被覆盖，不再出现
        assert!(
            !defs
                .iter()
                .any(|d| d.definition.description.contains("只读探索")
                    && d.source == Source::Builtin)
        );

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn list_definitions_sorted_by_name() {
        // 输出按 name 升序排序
        let temp = std::env::temp_dir().join("fuyao_test_list_defs_sorted");
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
        let defs = list_definitions(&ctx);
        let names: Vec<&str> = defs.iter().map(|d| d.id.as_str()).collect();
        // 升序：alpha 在 zebra 前
        assert!(names.windows(2).all(|w| w[0] <= w[1]));
        assert!(
            names.iter().position(|n| *n == "alpha") < names.iter().position(|n| *n == "zebra")
        );

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
