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
    let (frontmatter, body) = parse_frontmatter(&content);

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

    Some(AgentDefinition {
        name,
        description,
        version,
        author,
        mode,
        system_prompt: body.trim().to_string(),
        source_path: Some(file_path.to_string_lossy().to_string()),
    })
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

        // 未指定 mode 默认 All
        std::fs::write(&md, "---\nname: any\n---\n任意").unwrap();
        let def = load_agent_definition(&md).unwrap();
        assert_eq!(def.mode, fuyao_api::AgentMode::All);

        std::fs::remove_dir_all(&temp).ok();
    }
}
