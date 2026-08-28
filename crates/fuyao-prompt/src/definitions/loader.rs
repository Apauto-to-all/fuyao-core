//! Agent 定义加载器
//!
//! 按层加载 Agent 定义：单文件加载、内置默认表加载、四层优先级全链查找。
//! 解析核心见 [`super::parser`]。

use super::parser::parse_definition_from_content;
use fuyao_api::AgentDefinition;
use fuyao_api::AgentPaths;
use std::path::Path;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
