//! Agent 定义解析器
//!
//! 从 Markdown 文本解析 Agent 定义：frontmatter 提取元数据，body 作为系统提示词。
//! 纯函数、无 IO，供 [`super::loader`] 的文件加载与内置定义加载共用。

use fuyao_api::AgentDefinition;
use fuyao_api::AgentMode;
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

/// frontmatter 分隔解析正则
///
/// 模式为编译期常量，提升为进程级静态量只编译一次，避免每次解析定义文件重复编译。
static FRONTMATTER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)^---\s*\n(.*?)\n?---\s*\n(.*)$").expect("无效的 frontmatter 正则")
});

/// YAML 值的类型名（错误信息用）
fn value_type_desc(v: &serde_yaml::Value) -> &'static str {
    match v {
        serde_yaml::Value::Null => "null",
        serde_yaml::Value::Bool(_) => "布尔",
        serde_yaml::Value::Number(_) => "数字",
        serde_yaml::Value::String(_) => "字符串",
        serde_yaml::Value::Sequence(_) => "序列",
        serde_yaml::Value::Mapping(_) => "映射",
        serde_yaml::Value::Tagged(_) => "带标签值",
    }
}

/// 取字符串字段的值（name / description / version / author 共用）
///
/// 字段缺省（键不存在）返回空串；字段存在但值非字符串（含写了键没给值的 null）
/// 属于类型错误，返回 `Err`（中文错误信息含来源路径与原因）。
fn string_field(fm: &serde_yaml::Mapping, key: &str, source: &str) -> Result<String, String> {
    match fm.get(key) {
        None => Ok(String::new()),
        Some(v) => v.as_str().map(str::to_string).ok_or_else(|| {
            format!(
                "Agent 定义 `{source}` 的字段 `{key}` 类型错误：应为字符串，实际为{}",
                value_type_desc(v)
            )
        }),
    }
}

/// 从 Markdown 文本解析 Agent 定义
///
/// [`super::loader::load_agent_definition`]（文件路径）与
/// [`super::loader::load_builtin_definition`]（编译期嵌入文本）共用的解析核心。
/// frontmatter 提取元数据，body 作为系统提示词。
///
/// - `content`：完整 Markdown 文本（可含 frontmatter）
/// - `source_path`：来源文件路径（错误信息标识来源用），内置定义传 `None`
///
/// 一律返回 `Err`（中文错误信息含来源与原因）的情况：
/// - frontmatter YAML 语法错误或顶层非映射
/// - `mode` 值未知或非字符串
/// - `tools` 非「工具名=布尔」映射（值非布尔、键非字符串、整体非映射）
/// - name / description / version / author 字段存在但非字符串
///
/// 字段缺省的默认值：name / description / version / author 为空串，
/// mode 为 [`AgentMode::Primary`]，tools 为空表（= 全部启用，既有语义）。
pub(super) fn parse_definition_from_content(
    content: &str,
    source_path: Option<String>,
) -> Result<AgentDefinition, String> {
    // 错误信息的来源标识：用户文件带路径，内置定义用固定标识
    let source = source_path
        .clone()
        .unwrap_or_else(|| "<内置定义>".to_string());

    let (frontmatter, body) = parse_frontmatter(content)
        .map_err(|cause| format!("Agent 定义 `{source}` 的 frontmatter 解析失败：{cause}"))?;

    let name = string_field(&frontmatter, "name", &source)?;
    let description = string_field(&frontmatter, "description", &source)?;
    let version = string_field(&frontmatter, "version", &source)?;
    let author = string_field(&frontmatter, "author", &source)?;

    // mode：字段缺省仍默认主代理（Primary）；写了但值未知 / 非字符串才报错
    let mode = match frontmatter.get("mode") {
        None => AgentMode::default(),
        Some(v) => {
            let mode_str = v.as_str().ok_or_else(|| {
                format!(
                    "Agent 定义 `{source}` 的字段 `mode` 类型错误：应为字符串，实际为{}",
                    value_type_desc(v)
                )
            })?;
            AgentMode::parse(mode_str).map_err(|cause| format!("Agent 定义 `{source}`：{cause}"))?
        }
    };

    // tools：与全局 [tools.enabled] 同款语义（工具名=是否启用）。
    // 缺省（字段不存在）为空表 = 全部启用；字段存在则整体必须是「字符串键=布尔值」
    // 映射，任何一处类型不符（非映射 / 键非字符串 / 值非布尔）都报错。
    let tools = match frontmatter.get("tools") {
        None => HashMap::new(),
        Some(v) => {
            let mapping = v.as_mapping().ok_or_else(|| {
                format!(
                    "Agent 定义 `{source}` 的字段 `tools` 类型错误：应为「工具名=是否启用」映射，实际为{}",
                    value_type_desc(v)
                )
            })?;
            let mut tools = HashMap::with_capacity(mapping.len());
            for (k, val) in mapping {
                let tool_name = k.as_str().ok_or_else(|| {
                    format!(
                        "Agent 定义 `{source}` 的字段 `tools` 存在非字符串键（实际为{}）",
                        value_type_desc(k)
                    )
                })?;
                let enabled = val.as_bool().ok_or_else(|| {
                    format!(
                        "Agent 定义 `{source}` 的字段 `tools.{tool_name}` 类型错误：应为布尔，实际为{}",
                        value_type_desc(val)
                    )
                })?;
                tools.insert(tool_name.to_string(), enabled);
            }
            tools
        }
    };

    Ok(AgentDefinition {
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

/// 解析 frontmatter 格式（YAML + Markdown body）
///
/// 支持空 frontmatter（---\n---）与无 frontmatter 的纯正文（无 frontmatter 时
/// metadata 为空映射、body 为全文）。
/// YAML 语法错误（含顶层非映射）向上传播 `Err`，由调用方附来源路径后报出。
///
/// 返回 `(metadata_dict, body_content)`。
fn parse_frontmatter(content: &str) -> Result<(serde_yaml::Mapping, String), serde_yaml::Error> {
    if let Some(caps) = FRONTMATTER_RE.captures(content) {
        let fm_str = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let body = caps
            .get(2)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        Ok((parse_yaml_dict(fm_str)?, body))
    } else {
        Ok((serde_yaml::Mapping::new(), content.to_string()))
    }
}

/// 解析 YAML 字典
///
/// 空白输入视为空映射（对应空 frontmatter）；语法错误或顶层不是映射均返回 `Err`。
fn parse_yaml_dict(s: &str) -> Result<serde_yaml::Mapping, serde_yaml::Error> {
    if s.trim().is_empty() {
        return Ok(serde_yaml::Mapping::new());
    }
    serde_yaml::from_str::<serde_yaml::Mapping>(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_frontmatter_basic() {
        let content = "---\nname: test\ndescription: desc\n---\nBody content";
        let (fm, body) = parse_frontmatter(content).unwrap();
        assert_eq!(fm["name"], "test");
        assert_eq!(fm["description"], "desc");
        assert_eq!(body, "Body content");
    }

    #[test]
    fn parse_frontmatter_empty() {
        let content = "---\n---\nBody content";
        let (fm, body) = parse_frontmatter(content).unwrap();
        assert!(fm.is_empty());
        assert_eq!(body, "Body content");
    }

    #[test]
    fn parse_frontmatter_no_frontmatter() {
        let content = "Just some content";
        let (fm, body) = parse_frontmatter(content).unwrap();
        assert!(fm.is_empty());
        assert_eq!(body, "Just some content");
    }

    #[test]
    fn parse_frontmatter_broken_yaml_reports_error() {
        // frontmatter 存在但 YAML 语法坏 → Err（不静默当空映射）
        let content = "---\nname: [unclosed\n---\nBody";
        assert!(parse_frontmatter(content).is_err());
    }

    #[test]
    fn parse_frontmatter_non_mapping_top_level_reports_error() {
        // YAML 顶层是序列而非映射 → Err
        let content = "---\n- a\n- b\n---\nBody";
        assert!(parse_frontmatter(content).is_err());
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
        assert!(!def.tools.contains_key("grep"));
    }

    #[test]
    fn parse_definition_from_content_tools_default_empty() {
        // 无 tools 字段 → 空 map（无限制，向后兼容）
        let md = "---\nname: plain\n---\n普通定义";
        let def = parse_definition_from_content(md, None).unwrap();
        assert!(def.tools.is_empty());
    }

    #[test]
    fn parse_definition_from_content_tools_non_bool_value_reports_error() {
        // tools 的值非 bool → 报错（含字段名与来源路径）
        let md = "---\nname: mixed\ntools:\n  write: false\n  bad: \"not-a-bool\"\n---\n混合值";
        let err =
            parse_definition_from_content(md, Some("agents/mixed.md".to_string())).unwrap_err();
        assert!(
            err.contains("tools.bad") && err.contains("布尔"),
            "错误信息应指出字段与期望类型：{err}"
        );
        assert!(
            err.contains("agents/mixed.md"),
            "错误信息应含来源路径：{err}"
        );
    }

    #[test]
    fn parse_definition_from_content_tools_non_mapping_reports_error() {
        // tools 整体非映射 → 报错
        let md = "---\nname: bad\ntools: [write, edit]\n---\n序列形态";
        let err = parse_definition_from_content(md, None).unwrap_err();
        assert!(
            err.contains("`tools`") && err.contains("映射"),
            "错误信息应说明：{err}"
        );
    }

    #[test]
    fn parse_definition_from_content_invalid_yaml_reports_error() {
        // YAML 语法坏 → 报错（含来源路径）
        let md = "---\nname: [unclosed\nversion: 1.0\n---\n正文";
        let err =
            parse_definition_from_content(md, Some("agents/broken.md".to_string())).unwrap_err();
        assert!(
            err.contains("frontmatter 解析失败"),
            "错误信息应说明原因：{err}"
        );
        assert!(
            err.contains("agents/broken.md"),
            "错误信息应含来源路径：{err}"
        );
    }

    #[test]
    fn parse_definition_from_content_non_string_field_reports_error() {
        // 字段存在但类型不符（name 为数字）→ 报错
        let md = "---\nname: 123\n---\n正文";
        let err = parse_definition_from_content(md, None).unwrap_err();
        assert!(
            err.contains("`name`") && err.contains("字符串"),
            "错误信息应指出字段与期望类型：{err}"
        );

        // version 为布尔同样报错
        let md = "---\nname: ok\nversion: true\n---\n正文";
        let err = parse_definition_from_content(md, None).unwrap_err();
        assert!(err.contains("`version`"), "错误信息应指出字段：{err}");
    }

    #[test]
    fn parse_definition_from_content_default_field_values() {
        // 缺省字段默认值：name/description/version/author 空串、mode Primary、tools 空
        let md = "---\n---\n仅有正文";
        let def = parse_definition_from_content(md, None).unwrap();
        assert_eq!(def.name, "");
        assert_eq!(def.description, "");
        assert_eq!(def.version, "");
        assert_eq!(def.author, "");
        assert_eq!(def.mode, fuyao_api::AgentMode::Primary);
        assert!(def.tools.is_empty());
        assert_eq!(def.system_prompt, "仅有正文");
    }

    #[test]
    fn parse_definition_from_content_explicit_version_preserved() {
        // 文件显式写的 version 原样保留
        let md = "---\nname: with-version\nversion: 2.3.1\n---\n正文";
        let def = parse_definition_from_content(md, None).unwrap();
        assert_eq!(def.version, "2.3.1");
    }
}
