//! Agent 定义加载器
//!
//! 从 `agents/{name}.md` 文件加载 Agent 定义，支持 frontmatter 解析。
//! 定义文件集中在 `agents/` 文件夹管理，由 AgentConfig.definition 指定加载哪个定义。

use fuyao_api::AgentPaths;
use fuyao_api::{AgentDefinition, AgentMode};
use regex::Regex;
use std::collections::HashMap;
use std::path::Path;
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

/// 从定义文件（`agents/*.md`）加载 Agent 定义
///
/// 解析 frontmatter 获取元数据，body 作为系统提示词。
///
/// 返回值三分，供分层查找区分「未找到」与「文件损坏」：
/// - `Ok(Some(def))`：文件存在且解析成功
/// - `Ok(None)`：文件不存在（调用方继续下一层查找）
/// - `Err(cause)`：文件存在但读取或解析失败（损坏），中文错误含路径与原因——
///   调用方应立即上抛，静默跳过会把「文件损坏」伪装成「未找到」
pub fn load_agent_definition(file_path: &Path) -> Result<Option<AgentDefinition>, String> {
    let content = match std::fs::read_to_string(file_path) {
        Ok(c) => c,
        // 文件不存在与「存在但坏」是两种语义：不存在属正常未命中
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(format!(
                "读取 Agent 定义文件失败 `{}`：{e}",
                file_path.display()
            ));
        }
    };
    parse_definition_from_content(&content, Some(file_path.to_string_lossy().to_string())).map(Some)
}

/// 从 Markdown 文本解析 Agent 定义
///
/// [`load_agent_definition`]（文件路径）与 [`load_builtin_definition`]
/// （编译期嵌入文本）共用的解析核心。
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
pub(crate) fn parse_definition_from_content(
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

/// 加载内置默认 Agent 定义
///
/// 按 name 查 [`crate::builtin::builtin_agent_md`] 取编译期嵌入的 Markdown，再解析。
/// 覆盖链：用户 `agents/{name}.md` → 内置默认（本函数）。
///
/// 解析失败（内置文件格式错误）直接 panic：编译期嵌入内容受开发者完全掌控，
/// 解析失败属开发期 bug，应尽早暴露而非伪装成「未找到」。
///
/// 返回 `None`：name 不在内置表中（未知 name，由调用方决定错误语义）。
pub fn load_builtin_definition(name: &str) -> Option<AgentDefinition> {
    let md = crate::builtin::builtin_agent_md(name)?;
    Some(
        parse_definition_from_content(md, None)
            .expect("内置 Agent 定义解析失败：builtin/assets/agents/ 下的 .md 格式错误"),
    )
}

/// 从 AgentPaths 加载 Agent 定义
///
/// 纯查找函数：按 name 沿 `agents/` 目录四层优先级（workspace > agent > global >
/// extra）匹配定义文件，用户层全部未命中再查内置表（default/explore/executor）。
///
/// 返回值三分，调用方须区分「未知名」与「文件损坏」两种失败：
/// - `Ok(Some(def))`：命中（用户文件或内置表）
/// - `Ok(None)`：未知名（四层目录与内置表均无此文件）——错误语义（附可用列表的
///   报错）由 [`crate::resolve_definition`] 统一收口
/// - `Err(cause)`：某层定义文件存在但损坏（读取 / 解析失败），立即上抛给调用方——
///   不静默跳下一层或内置表，避免「文件损坏」被伪装成「未知名」或意外落回内置
///
/// name 由 AgentConfig.definition 提供（必填）。
/// 路径方法 `agents_def_paths(&self, name)` 负责解析具体路径。
pub fn load_agent_definition_from_agent_paths(
    agent_paths: &AgentPaths,
    name: &str,
) -> Result<Option<AgentDefinition>, String> {
    let paths = agent_paths.agents_def_paths(name);

    for path in paths.all() {
        if let Some(def) = load_agent_definition(path)? {
            return Ok(Some(def));
        }
    }

    // 用户文件未命中 → 查内置默认表（default/explore/executor）
    Ok(load_builtin_definition(name))
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
    fn load_agent_definition_not_found() {
        // 文件不存在 → Ok(None)（与「存在但损坏」的 Err 可区分）
        let result = load_agent_definition(Path::new("nonexistent/path/missing.md"));
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn load_agent_definition_corrupted_file_reports_error() {
        // 文件存在但解析失败 → Err（错误含来源路径）
        let temp = std::env::temp_dir().join("fuyao_test_loader_corrupted_single");
        std::fs::create_dir_all(&temp).unwrap();
        let md = temp.join("broken.md");
        std::fs::write(&md, "---\nname: [unclosed\n---\n正文").unwrap();

        let err = load_agent_definition(&md).unwrap_err();
        assert!(err.contains("解析失败"), "错误信息应说明原因：{err}");
        assert!(
            err.contains(md.to_string_lossy().as_ref()),
            "错误信息应含来源文件路径：{err}"
        );

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn load_agent_definition_from_agent_paths_hits_builtin_default() {
        // 无 agents/default.md 时命中内置 default（name = "fuyao"）
        let ctx = AgentPaths::default();
        let def = load_agent_definition_from_agent_paths(&ctx, "default")
            .unwrap()
            .expect("应命中内置 default");
        assert_eq!(def.name, "fuyao");
    }

    #[test]
    fn load_agent_definition_from_agent_paths_loads_default_md() {
        // 通过 extra_dirs 注入 agents/default.md，验证覆盖内置 default
        let temp = std::env::temp_dir().join("fuyao_test_loader_default_extra");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        let md = "---\nname: my-agent\ndescription: test\n---\n# 自定义默认\n你是测试Agent。";
        std::fs::write(plugin.join("agents").join("default.md"), md).unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let def = load_agent_definition_from_agent_paths(&ctx, "default")
            .unwrap()
            .expect("extra 层 default.md 应可加载");
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
        let def = load_agent_definition_from_agent_paths(&ctx, "reviewer")
            .unwrap()
            .expect("reviewer.md 应可加载");
        assert_eq!(def.name, "reviewer-agent");
        assert!(def.system_prompt.contains("代码审查"));
        // name = "default" → 加载 default.md
        let def = load_agent_definition_from_agent_paths(&ctx, "default")
            .unwrap()
            .expect("default.md 应可加载");
        assert_eq!(def.name, "default-agent");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn load_agent_definition_from_agent_paths_unknown_name_returns_ok_none() {
        // name 对应文件不存在 → Ok(None)（错误语义由 resolve_definition 收口，此处不兜底）
        let ctx = AgentPaths::default();
        let result = load_agent_definition_from_agent_paths(&ctx, "nonexistent");
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn load_agent_definition_from_agent_paths_corrupted_file_reports_error_immediately() {
        // 文件存在但损坏 → 立即 Err，不静默跳过、不落内置表
        let temp = std::env::temp_dir().join("fuyao_test_loader_corrupted_layer");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();
        std::fs::write(
            plugin.join("agents").join("default.md"),
            "---\nname: [unclosed\n---\n坏掉的正文",
        )
        .unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let err = load_agent_definition_from_agent_paths(&ctx, "default").unwrap_err();
        // 错误指向损坏的用户文件路径，而非内置定义
        assert!(
            err.contains(
                plugin
                    .join("agents")
                    .join("default.md")
                    .to_string_lossy()
                    .as_ref()
            ),
            "错误信息应含损坏文件路径：{err}"
        );

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn load_agent_definition_from_agent_paths_skips_absent_layers() {
        // 各层文件不存在（仅目录存在）→ 继续下一层直至内置表，不报错
        let temp = std::env::temp_dir().join("fuyao_test_loader_absent_layers");
        let plugin = temp.join("plugin");
        std::fs::create_dir_all(plugin.join("agents")).unwrap();

        let ctx = AgentPaths {
            extra_dirs: vec![plugin.clone()],
            ..Default::default()
        };
        let result = load_agent_definition_from_agent_paths(&ctx, "default");
        assert!(result.is_ok(), "层内无文件属正常未命中，不应报错");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn load_agent_definition_from_agent_paths_ignores_agent_id() {
        // agent_id 与 definition 正交：定义加载由 name 参数决定，与 agent_id 无关
        let ctx = AgentPaths {
            agent_id: Some("global/coder".to_string()),
            ..Default::default()
        };
        let def = load_agent_definition_from_agent_paths(&ctx, "default")
            .unwrap()
            .expect("应命中内置 default");
        // 无 agents/default.md → 命中内置 default，与 agent_id 无关
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
        let def = load_agent_definition(&md)
            .unwrap()
            .expect("subagent 定义应可加载");
        assert_eq!(def.mode, fuyao_api::AgentMode::Subagent);

        // primary 模式
        std::fs::write(&md, "---\nname: main\nmode: primary\n---\n你是主代理").unwrap();
        let def = load_agent_definition(&md)
            .unwrap()
            .expect("primary 定义应可加载");
        assert_eq!(def.mode, fuyao_api::AgentMode::Primary);

        // 未写 mode 字段 → 默认 Primary
        std::fs::write(&md, "---\nname: any\n---\n任意").unwrap();
        let def = load_agent_definition(&md)
            .unwrap()
            .expect("缺省 mode 定义应可加载");
        assert_eq!(def.mode, fuyao_api::AgentMode::Primary);

        // 写了 mode 但值未知 → 报错
        std::fs::write(&md, "---\nname: bad\nmode: both\n---\n未知模式").unwrap();
        let err = load_agent_definition(&md).unwrap_err();
        assert!(err.contains("mode 值未知"), "错误信息应说明原因：{err}");
        assert!(err.contains("both"), "错误信息应含未知值：{err}");

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
        // explore 只读收窄：禁用 write/edit，未列出的（read/glob/grep/bash/webfetch/...）默认启用
        assert_eq!(explore.tools.get("write"), Some(&false));
        assert_eq!(explore.tools.get("edit"), Some(&false));
        assert!(!explore.tools.contains_key("read"));

        let executor = load_builtin_definition("executor").unwrap();
        assert_eq!(executor.name, "executor");
        assert_eq!(executor.mode, fuyao_api::AgentMode::Subagent);
        // executor 通用执行：不声明 tools = 全开（未列出默认启用）
        assert!(executor.tools.is_empty());
    }

    #[test]
    fn load_builtin_definition_unknown_name() {
        assert!(load_builtin_definition("nonexistent").is_none());
    }

    #[test]
    fn load_agent_definition_from_agent_paths_hits_builtin_subagent() {
        // 用户无 agents/explore.md → 命中内置 explore
        let ctx = AgentPaths::default();
        let def = load_agent_definition_from_agent_paths(&ctx, "explore")
            .unwrap()
            .expect("应命中内置 explore");
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
        let def = load_agent_definition_from_agent_paths(&ctx, "explore")
            .unwrap()
            .expect("用户 explore.md 应可加载");
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
