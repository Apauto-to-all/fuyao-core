//! Agent 定义加载器
//!
//! 从 `agents/{name}.md` 文件加载 Agent 定义，支持 frontmatter 解析。
//! 定义文件集中在 `agents/` 文件夹管理，由 AgentConfig.definition 指定加载哪个定义。

use crate::default::DEFAULT_FUYAO_AGENT;
use fuyao_api::AgentPaths;
use fuyao_api::{AgentDefinition, AgentMode};
use regex::Regex;
use std::path::Path;

/// 从定义文件（`agents/*.md`）加载 Agent 定义
///
/// 解析 frontmatter 获取元数据，body 作为系统提示词。
///
/// 返回 `None` 的情况：文件读取失败或解析失败。
pub fn load_agent_definition(file_path: &Path) -> Option<AgentDefinition> {
    let content = std::fs::read_to_string(file_path).ok()?;
    parse_definition_from_content(&content, Some(file_path.to_string_lossy().to_string()))
}

/// 从 Markdown 文本解析 Agent 定义
///
/// [`load_agent_definition`]（文件路径）与 [`default::DEFAULT_FUYAO_AGENT`] /
/// [`load_builtin_definition`]（编译期嵌入文本）共用的解析核心。
/// frontmatter 提取元数据，body 作为系统提示词。
///
/// - `content`：完整 Markdown 文本（可含 frontmatter）
/// - `source_path`：来源文件路径，解析失败或无来源时传 `None`
///
/// 返回 `None` 的情况：解析失败（frontmatter 格式错误等）。
pub(crate) fn parse_definition_from_content(
    content: &str,
    source_path: Option<String>,
) -> Option<AgentDefinition> {
    let (frontmatter, body) = parse_frontmatter(content);

    let name = frontmatter
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let description = frontmatter
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let version = frontmatter
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("1.0.0")
        .to_string();
    let author = frontmatter
        .get("author")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mode = frontmatter
        .get("mode")
        .and_then(|v| v.as_str())
        .map(AgentMode::from)
        .unwrap_or_default();

    // tools：与全局 [tools.enabled] 同款语义（工具名=是否启用）。
    // frontmatter 是 serde_yaml::Mapping，tools 值为嵌套 Mapping；逐 key 取 bool，
    // 非 bool 值（格式错误）静默跳过——值类型错误由用户承担，解析层不阻断。
    let tools = frontmatter
        .get("tools")
        .and_then(|v| v.as_mapping())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    let name = k.as_str()?;
                    let enabled = v.as_bool()?;
                    Some((name.to_string(), enabled))
                })
                .collect()
        })
        .unwrap_or_default();

    Some(AgentDefinition {
        name,
        description,
        version,
        author,
        mode,
        tools,
        system_prompt: body.trim().to_string(),
        source_path,
    })
}

/// 加载内置默认 Agent 定义
///
/// 按 name 查 [`default::builtin_definition_md`] 取编译期嵌入的 Markdown，再解析。
/// 覆盖链：用户 `agents/{name}.md` → 内置默认（本函数）→ [`default::DEFAULT_FUYAO_AGENT`]。
///
/// 返回 `None`：name 不在内置默认表中（未知 name，由调用方决定是否回退到 DEFAULT_FUYAO_AGENT）。
pub fn load_builtin_definition(name: &str) -> Option<AgentDefinition> {
    let md = crate::default::builtin_definition_md(name)?;
    parse_definition_from_content(md, None)
}

/// 从 AgentPaths 加载 Agent 定义
///
/// 从 `agents/` 文件夹按 name 匹配定义文件，三层优先级：workspace → global → extra。
/// 文件不存在或解析失败时，返回默认定义。
///
/// name 由 AgentConfig.definition 提供（None 时调用方传 `"default"`）。
/// 路径方法 `agents_def_paths(&self, name)` 负责解析具体路径。
pub fn load_agent_definition_from_agent_paths(
    agent_paths: &AgentPaths,
    name: &str,
) -> AgentDefinition {
    let paths = agent_paths.agents_def_paths(name);

    for path in paths.all() {
        if path.exists()
            && let Some(def) = load_agent_definition(path)
        {
            return def;
        }
    }

    // 用户文件未命中 → 查内置默认表（default/explore/executor）
    if let Some(builtin) = load_builtin_definition(name) {
        return builtin;
    }

    // 内置也没有该 name → 回退主 Agent 默认定义
    DEFAULT_FUYAO_AGENT.clone()
}

/// 解析 frontmatter 格式（YAML + Markdown body）
///
/// 支持空 frontmatter（---\n---）。
///
/// 返回 `(metadata_dict, body_content)`，如果没有 frontmatter 返回空 HashMap。
fn parse_frontmatter(content: &str) -> (serde_yaml::Mapping, String) {
    let pattern = Regex::new(r"(?s)^---\s*\n(.*?)\n?---\s*\n(.*)$").unwrap();

    if let Some(caps) = pattern.captures(content) {
        let fm_str = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let body = caps
            .get(2)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        (parse_yaml_dict(fm_str), body)
    } else {
        (serde_yaml::Mapping::new(), content.to_string())
    }
}

/// 解析 YAML 字典，失败返回空 Mapping
fn parse_yaml_dict(s: &str) -> serde_yaml::Mapping {
    if s.trim().is_empty() {
        return serde_yaml::Mapping::new();
    }
    match serde_yaml::from_str::<serde_yaml::Mapping>(s) {
        Ok(m) => m,
        Err(_) => serde_yaml::Mapping::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_frontmatter_basic() {
        let content = "---\nname: test\ndescription: desc\n---\nBody content";
        let (fm, body) = parse_frontmatter(content);
        assert_eq!(fm["name"], "test");
        assert_eq!(fm["description"], "desc");
        assert_eq!(body, "Body content");
    }

    #[test]
    fn parse_frontmatter_empty() {
        let content = "---\n---\nBody content";
        let (fm, body) = parse_frontmatter(content);
        assert!(fm.is_empty());
        assert_eq!(body, "Body content");
    }

    #[test]
    fn parse_frontmatter_no_frontmatter() {
        let content = "Just some content";
        let (fm, body) = parse_frontmatter(content);
        assert!(fm.is_empty());
        assert_eq!(body, "Just some content");
    }

    #[test]
    fn load_agent_definition_not_found() {
        let result = load_agent_definition(Path::new("nonexistent/path/system.md"));
        assert!(result.is_none());
    }

    #[test]
    fn load_agent_definition_from_agent_paths_returns_default() {
        // 无 agents/default.md 时回退 DEFAULT_FUYAO_AGENT（name = "fuyao"）
        let ctx = AgentPaths::default();
        let def = load_agent_definition_from_agent_paths(&ctx, "default");
        assert_eq!(def.name, "fuyao");
    }

    #[test]
    fn load_agent_definition_from_agent_paths_loads_default_md() {
        // 通过 extra_dirs 注入 agents/default.md，验证覆盖 DEFAULT_FUYAO_AGENT
        let temp = std::env::temp_dir().join("fuyao_test_loader_default_extra");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        let md = "---\nname: my-agent\ndescription: test\n---\n# 自定义默认\n你是测试Agent。";
        std::fs::write(plugin.join("agents").join("default.md"), md).unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let def = load_agent_definition_from_agent_paths(&ctx, "default");
        // extra 层（无 global/workspace 覆盖时）的 default.md 被加载
        assert_eq!(def.name, "my-agent");
        assert!(def.system_prompt.contains("自定义默认"));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn load_agent_definition_from_agent_paths_loads_named_definition() {
        // 通过 extra_dirs 注入 agents/reviewer.md，验证 name 参数选择正确文件
        let temp = std::env::temp_dir().join("fuyao_test_loader_named");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        std::fs::write(
            plugin.join("agents").join("default.md"),
            "---\nname: default-agent\n---\n默认",
        )
        .unwrap();
        std::fs::write(
            plugin.join("agents").join("reviewer.md"),
            "---\nname: reviewer-agent\ndescription: code review\n---\n你是代码审查专家",
        )
        .unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        // name = "reviewer" → 加载 reviewer.md
        let def = load_agent_definition_from_agent_paths(&ctx, "reviewer");
        assert_eq!(def.name, "reviewer-agent");
        assert!(def.system_prompt.contains("代码审查"));
        // name = "default" → 加载 default.md
        let def = load_agent_definition_from_agent_paths(&ctx, "default");
        assert_eq!(def.name, "default-agent");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn load_agent_definition_from_agent_paths_named_not_found_falls_back() {
        // name 对应文件不存在 → 回退 DEFAULT_FUYAO_AGENT
        let ctx = AgentPaths::default();
        let def = load_agent_definition_from_agent_paths(&ctx, "nonexistent");
        assert_eq!(def.name, "fuyao");
    }

    #[test]
    fn load_agent_definition_from_agent_paths_ignores_agent_id() {
        // agent_id 与 definition 正交：定义加载由 name 参数决定，与 agent_id 无关
        let ctx = AgentPaths {
            agent_id: Some("global/coder".to_string()),
            ..Default::default()
        };
        let def = load_agent_definition_from_agent_paths(&ctx, "default");
        // 无 agents/default.md → 回退默认，与 agent_id 无关
        assert_eq!(def.name, "fuyao");
    }

    #[test]
    fn load_agent_definition_parses_mode() {
        let temp = std::env::temp_dir().join("fuyao_test_loader_mode");
        std::fs::create_dir_all(&temp).unwrap();
        let md = temp.join("test.md");

        // subagent 模式
        std::fs::write(
            &md,
            "---\nname: sub\ndescription: sub\nmode: subagent\n---\n你是子代理",
        )
        .unwrap();
        let def = load_agent_definition(&md).unwrap();
        assert_eq!(def.mode, fuyao_api::AgentMode::Subagent);

        // primary 模式
        std::fs::write(&md, "---\nname: main\nmode: primary\n---\n你是主代理").unwrap();
        let def = load_agent_definition(&md).unwrap();
        assert_eq!(def.mode, fuyao_api::AgentMode::Primary);

        // 未指定 mode 默认 Primary
        std::fs::write(&md, "---\nname: any\n---\n任意").unwrap();
        let def = load_agent_definition(&md).unwrap();
        assert_eq!(def.mode, fuyao_api::AgentMode::Primary);

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn load_builtin_definition_known_names() {
        let def = load_builtin_definition("default").unwrap();
        assert_eq!(def.name, "fuyao");
        assert!(!def.system_prompt.is_empty());

        let explore = load_builtin_definition("explore").unwrap();
        assert_eq!(explore.name, "explore");
        assert_eq!(explore.mode, fuyao_api::AgentMode::Subagent);

        let executor = load_builtin_definition("executor").unwrap();
        assert_eq!(executor.name, "executor");
        assert_eq!(executor.mode, fuyao_api::AgentMode::Subagent);
    }

    #[test]
    fn load_builtin_definition_unknown_name() {
        assert!(load_builtin_definition("nonexistent").is_none());
    }

    #[test]
    fn load_agent_definition_from_agent_paths_falls_back_to_builtin_subagent() {
        // 用户无 agents/explore.md → 回退内置 explore（而非 DEFAULT_FUYAO_AGENT）
        let ctx = AgentPaths::default();
        let def = load_agent_definition_from_agent_paths(&ctx, "explore");
        assert_eq!(def.name, "explore");
        assert_eq!(def.mode, fuyao_api::AgentMode::Subagent);
    }

    #[test]
    fn load_agent_definition_from_agent_paths_user_overrides_builtin() {
        // 用户 agents/explore.md 覆盖内置
        let temp = std::env::temp_dir().join("fuyao_test_loader_override_builtin");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        std::fs::write(
            plugin.join("agents").join("explore.md"),
            "---\nname: my-explore\ndescription: custom\nmode: subagent\n---\n自定义探索",
        )
        .unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let def = load_agent_definition_from_agent_paths(&ctx, "explore");
        assert_eq!(def.name, "my-explore");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn parse_definition_from_content_parses_tools() {
        // frontmatter 带 tools 字段，与全局 [tools.enabled] 同款语义
        let md = "---\nname: researcher\nmode: subagent\ntools:\n  write: false\n  edit: false\n  bash: false\n  read: true\n---\n你是只读研究员";
        let def = parse_definition_from_content(md, None).unwrap();
        assert_eq!(def.tools.len(), 4);
        assert_eq!(def.tools.get("write"), Some(&false));
        assert_eq!(def.tools.get("edit"), Some(&false));
        assert_eq!(def.tools.get("bash"), Some(&false));
        assert_eq!(def.tools.get("read"), Some(&true));
        // 未列出的工具不进 map（默认启用语义）
        assert!(def.tools.get("grep").is_none());
    }

    #[test]
    fn parse_definition_from_content_tools_default_empty() {
        // 无 tools 字段 → 空 map（无限制，向后兼容）
        let md = "---\nname: plain\n---\n普通定义";
        let def = parse_definition_from_content(md, None).unwrap();
        assert!(def.tools.is_empty());
    }

    #[test]
    fn parse_definition_from_content_tools_skips_non_bool_values() {
        // 非 bool 值（格式错误）静默跳过，不阻断解析
        let md = "---\nname: mixed\ntools:\n  write: false\n  bad: \"not-a-bool\"\n  read: true\n---\n混合值";
        let def = parse_definition_from_content(md, None).unwrap();
        // bad 被跳过，只留 write / read
        assert_eq!(def.tools.len(), 2);
        assert_eq!(def.tools.get("write"), Some(&false));
        assert_eq!(def.tools.get("read"), Some(&true));
        assert!(def.tools.get("bad").is_none());
    }
}
