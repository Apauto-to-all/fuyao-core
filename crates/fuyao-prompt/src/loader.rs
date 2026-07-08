//! Agent 定义加载器
//!
//! 从 `agents/{name}.md` 文件加载 Agent 定义，支持 frontmatter 解析。
//! 定义文件集中在 `agents/` 文件夹管理，本期固定加载 `agents/default.md`。

use crate::default::DEFAULT_FUYAO_AGENT;
use fuyao_api::AgentPaths;
use fuyao_api::prompt_types::AgentDefinition;
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
/// 从 `agents/` 文件夹按 name 匹配定义文件，三层优先级：workspace → global → extra。
/// 文件不存在或解析失败时，返回默认定义。
///
/// **本期固定加载 `default` 定义**：所有独立 Agent 共用同一份定义。
///
/// `// TODO:` 后续需设计「独立 Agent（agent_id）如何选择加载哪个定义提示词」的机制，
/// 届时 name 来源改变（不能简单用 agent_id 当选择键，二者层级不同）。
/// 路径方法 `agents_def_paths(&self, name)` 已预留 name 参数作为通用接口。
pub fn load_agent_definition_from_agent_paths(agent_paths: &AgentPaths) -> AgentDefinition {
    // 本期固定加载系统默认定义，与 agent_id 完全无关
    // TODO: 后续设计定义选择机制，支持加载任意的 Agent 定义提示词
    const DEFAULT_DEFINITION_NAME: &str = "default";

    let paths = agent_paths.agents_def_paths(DEFAULT_DEFINITION_NAME);

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
        let def = load_agent_definition_from_agent_paths(&ctx);
        assert_eq!(def.name, "fuyao");
    }

    #[test]
    fn load_agent_definition_from_agent_paths_loads_default_md() {
        // 通过 extra_dirs 注入 agents/default.md，验证覆盖 DEFAULT_FUYAO_AGENT
        // （不用 FUYAO_HOME，避免并发测试环境变量竞争）
        let temp = std::env::temp_dir().join("fuyao_test_loader_default_extra");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        let md = "---\nname: my-agent\ndescription: test\n---\n# 自定义默认\n你是测试Agent。";
        std::fs::write(plugin.join("agents").join("default.md"), md).unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let def = load_agent_definition_from_agent_paths(&ctx);
        // extra 层（无 global/workspace 覆盖时）的 default.md 被加载
        assert_eq!(def.name, "my-agent");
        assert!(def.system_prompt.contains("自定义默认"));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn load_agent_definition_from_agent_paths_ignores_agent_id() {
        // 即使指定 agent_id，定义加载仍固定为 default，与 agent_id 无关
        let ctx = AgentPaths {
            agent_id: Some("global/coder".to_string()),
            ..Default::default()
        };
        let def = load_agent_definition_from_agent_paths(&ctx);
        // 无 agents/default.md → 回退默认，与 agent_id 无关
        assert_eq!(def.name, "fuyao");
    }
}
