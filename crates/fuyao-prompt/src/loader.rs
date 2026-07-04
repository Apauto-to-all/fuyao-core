//! Agent 定义加载器
//!
//! 从 system.md 文件加载 Agent 定义，支持 frontmatter 解析。
//! 一个 Agent 目录 = 一个 system.md 文件。

use crate::default::DEFAULT_FUYAO_AGENT;
use fuyao_api::AgentPaths;
use fuyao_api::prompt_types::AgentDefinition;
use regex::Regex;
use std::path::Path;

/// 从 system.md 文件加载 Agent 定义
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

    Some(AgentDefinition {
        name,
        description,
        version,
        author,
        system_prompt: body.trim().to_string(),
        source_path: Some(file_path.to_string_lossy().to_string()),
    })
}

/// 从 AgentPaths 加载 Agent 定义
///
/// 从 system_md_paths 的三层路径查找，优先级：agent → global。
/// 文件不存在或解析失败时，返回默认定义。
pub fn load_agent_definition_from_agent_paths(agent_paths: &AgentPaths) -> AgentDefinition {
    let paths = agent_paths.system_md_paths();

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
        let ctx = AgentPaths::default();
        let def = load_agent_definition_from_agent_paths(&ctx);
        // 默认定义应该有默认名称
        assert_eq!(def.name, "fuyao");
    }
}
