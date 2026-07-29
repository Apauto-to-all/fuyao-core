//! fuyao-prompt 集成测试：系统提示词分层组装 + Agent 定义加载
//!
//! 聚焦跨模块协作与公开 API 契约：
//! - `build_system_prompt`：分层组装（覆盖区 + 补充区）的端到端拼接
//! - `load_agent_definition_from_agent_paths`：三层优先级解析（workspace → global → extra）
//! - 字段注入隔离：所有 AgentPaths.fuyao_home 指向 tempdir，零环境变量依赖

mod common;

use common::{
    make_agent_paths, temp_home, write_agent_def, write_agents_md, write_default_agent,
    write_instruction,
};
use fuyao_api::{AgentConfig, AgentMode};
use fuyao_prompt::{PromptUsage, build_system_prompt, load_agent_definition_from_agent_paths};

/// 测试默认用途（主 Agent）
const PRIMARY: PromptUsage = PromptUsage::Primary;

// ============================================================================
// build_system_prompt：分层组装
// ============================================================================

#[test]
fn build_system_prompt_empty_home_produces_default_sections() {
    // 无任何文件时，仍能组装出含 Agent 定义 + 环境的提示词
    let home = temp_home();
    let paths = make_agent_paths(home.path().to_path_buf(), None, Vec::new());

    let prompt = build_system_prompt(&paths, &AgentConfig::default(), PRIMARY);

    assert!(!prompt.is_empty());
    assert!(prompt.contains("# Agent 定义"), "应包含 Agent 定义 section");
    assert!(prompt.contains("# 环境"), "应包含环境 section");
    assert!(prompt.contains("Fuyao"), "默认 Agent 定义应含 Fuyao 标识");
}

#[test]
fn build_system_prompt_custom_default_agent_overrides_builtin() {
    // fuyao_home/agents/default.md 存在时，覆盖内置 DEFAULT_FUYAO_AGENT
    let home = temp_home();
    write_default_agent(
        home.path(),
        "---\nname: custom\ndescription: 自定义默认\n---\n你是自定义 Agent。",
    );
    let paths = make_agent_paths(home.path().to_path_buf(), None, Vec::new());

    let prompt = build_system_prompt(&paths, &AgentConfig::default(), PRIMARY);

    assert!(
        prompt.contains("自定义 Agent"),
        "自定义 default.md 应覆盖内置默认"
    );
}

#[test]
fn build_system_prompt_named_definition_selected_by_config() {
    // AgentConfig.definition 指定加载 reviewer.md 而非 default.md
    let home = temp_home();
    write_default_agent(home.path(), "---\nname: default\n---\n默认内容");
    write_agent_def(
        home.path(),
        "reviewer",
        "---\nname: reviewer\ndescription: 审查\n---\n你是代码审查专家",
    );
    let config = AgentConfig {
        definition: Some("reviewer".to_string()),
    };
    let paths = make_agent_paths(home.path().to_path_buf(), None, Vec::new());

    let prompt = build_system_prompt(&paths, &config, PRIMARY);

    assert!(prompt.contains("代码审查专家"), "应加载 reviewer 定义");
    assert!(!prompt.contains("默认内容"), "不应回退到 default.md 内容");
}

#[test]
fn build_system_prompt_includes_project_context_from_workspace() {
    // 工作目录有 AGENTS.md 时，项目上下文 section 应含其内容
    let home = temp_home();
    let ws = tempfile::tempdir().expect("创建工作目录失败");
    write_agents_md(ws.path(), "# 项目规范\n禁止使用 unsafe");
    let paths = make_agent_paths(
        home.path().to_path_buf(),
        Some(ws.path().to_path_buf()),
        Vec::new(),
    );

    let prompt = build_system_prompt(&paths, &AgentConfig::default(), PRIMARY);

    assert!(prompt.contains("# 项目上下文"), "应含项目上下文 section");
    assert!(prompt.contains("禁止使用 unsafe"), "应含 AGENTS.md 正文");
}

#[test]
fn build_system_prompt_includes_instructions_from_extra_dirs() {
    // extra_dirs 注入 instructions/ 文件夹，补充指令 section 应被组装
    let home = temp_home();
    let plugin = tempfile::tempdir().expect("创建插件目录失败");
    write_instruction(plugin.path(), "rule.md", "## 测试补充规则\n必须覆盖");
    let paths = make_agent_paths(
        home.path().to_path_buf(),
        None,
        vec![plugin.path().to_path_buf()],
    );

    let prompt = build_system_prompt(&paths, &AgentConfig::default(), PRIMARY);

    assert!(prompt.contains("# 补充指令"), "应含补充指令 section");
    assert!(prompt.contains("测试补充规则"), "应含指令正文");
}

#[test]
fn build_system_prompt_section_order_agent_before_env() {
    // 固定顺序契约：Agent 定义 → (项目上下文) → (工具指南) → (技能) → (补充指令) → 环境
    let home = temp_home();
    let paths = make_agent_paths(home.path().to_path_buf(), None, Vec::new());

    let prompt = build_system_prompt(&paths, &AgentConfig::default(), PRIMARY);

    let agent_idx = prompt.find("# Agent 定义").expect("应含 Agent 定义");
    let env_idx = prompt.find("# 环境").expect("应含环境");
    assert!(agent_idx < env_idx, "Agent 定义必须在环境之前");
}

#[test]
fn build_system_prompt_datetime_is_non_deterministic_but_present() {
    // 时间 section 无法注入，只断言前缀（不测精确值）
    let home = temp_home();
    let paths = make_agent_paths(home.path().to_path_buf(), None, Vec::new());

    let prompt = build_system_prompt(&paths, &AgentConfig::default(), PRIMARY);

    assert!(
        prompt.contains("当前时间："),
        "应含当前时间前缀（值非确定性，仅断言前缀）"
    );
}

#[test]
fn build_system_prompt_workspace_context_layered_subheaders() {
    // 工作目录 + fuyao_home 均有 AGENTS.md 时，项目上下文应含多级子标题
    let home = temp_home();
    write_agents_md(home.path(), "全局层上下文");
    let ws = tempfile::tempdir().expect("创建工作目录失败");
    write_agents_md(ws.path(), "项目层上下文");
    let paths = make_agent_paths(
        home.path().to_path_buf(),
        Some(ws.path().to_path_buf()),
        Vec::new(),
    );

    let prompt = build_system_prompt(&paths, &AgentConfig::default(), PRIMARY);

    assert!(prompt.contains("项目层上下文"), "应含项目层 AGENTS.md");
    assert!(prompt.contains("全局层上下文"), "应含全局层 AGENTS.md");
}

// ============================================================================
// load_agent_definition_from_agent_paths：三层优先级
// ============================================================================

#[test]
fn load_definition_falls_back_to_builtin_default_when_no_file() {
    // 无任何定义文件时，返回内置 DEFAULT_FUYAO_AGENT（name = "fuyao"）
    let home = temp_home();
    let paths = make_agent_paths(home.path().to_path_buf(), None, Vec::new());

    let def = load_agent_definition_from_agent_paths(&paths, "default");

    assert_eq!(def.name, "fuyao", "无文件时回退内置默认");
    assert_eq!(def.mode, AgentMode::Primary);
}

#[test]
fn load_definition_global_default_md_overrides_builtin() {
    // fuyao_home/agents/default.md 存在时，覆盖内置默认
    let home = temp_home();
    write_default_agent(
        home.path(),
        "---\nname: my-agent\ndescription: 自定义\n---\n你是测试 Agent",
    );
    let paths = make_agent_paths(home.path().to_path_buf(), None, Vec::new());

    let def = load_agent_definition_from_agent_paths(&paths, "default");

    assert_eq!(def.name, "my-agent");
    assert!(def.system_prompt.contains("测试 Agent"));
}

#[test]
fn load_definition_workspace_overrides_global() {
    // 三层优先级：workspace > global > extra
    let home = temp_home();
    write_agent_def(
        home.path(),
        "reviewer",
        "---\nname: global-reviewer\n---\n全局层定义",
    );
    let ws = tempfile::tempdir().expect("创建工作目录失败");
    // workspace/.fuyao/agents/reviewer.md（get_workspace_root 映射到 {ws}/.fuyao/）
    let ws_agents = ws.path().join(".fuyao").join("agents");
    std::fs::create_dir_all(&ws_agents).unwrap();
    std::fs::write(
        ws_agents.join("reviewer.md"),
        "---\nname: ws-reviewer\n---\n项目层定义",
    )
    .unwrap();
    let paths = make_agent_paths(
        home.path().to_path_buf(),
        Some(ws.path().to_path_buf()),
        Vec::new(),
    );

    let def = load_agent_definition_from_agent_paths(&paths, "reviewer");

    assert_eq!(def.name, "ws-reviewer", "workspace 层应优先于 global");
    assert!(def.system_prompt.contains("项目层定义"));
}

#[test]
fn load_definition_extra_layer_used_when_no_global_or_workspace() {
    // extra_dirs 提供的定义在 global/workspace 缺失时被加载
    let home = temp_home();
    let plugin = tempfile::tempdir().expect("创建插件目录失败");
    let plugin_agents = plugin.path().join("agents");
    std::fs::create_dir_all(&plugin_agents).unwrap();
    std::fs::write(
        plugin_agents.join("helper.md"),
        "---\nname: plugin-helper\ndescription: 来自插件\n---\n插件提供的助手",
    )
    .unwrap();
    let paths = make_agent_paths(
        home.path().to_path_buf(),
        None,
        vec![plugin.path().to_path_buf()],
    );

    let def = load_agent_definition_from_agent_paths(&paths, "helper");

    assert_eq!(def.name, "plugin-helper");
    assert!(def.system_prompt.contains("插件提供的助手"));
}

#[test]
fn load_definition_named_not_found_falls_back_to_builtin() {
    // name 对应文件不存在于任何层 → 回退内置默认（而非报错）
    let home = temp_home();
    let paths = make_agent_paths(home.path().to_path_buf(), None, Vec::new());

    let def = load_agent_definition_from_agent_paths(&paths, "nonexistent");

    assert_eq!(def.name, "fuyao", "命名定义缺失时回退内置默认");
}

#[test]
fn load_definition_parses_mode_field() {
    // frontmatter 的 mode 字段正确映射到 AgentMode
    let home = temp_home();
    write_agent_def(
        home.path(),
        "sub",
        "---\nname: sub\ndescription: 子\nmode: subagent\n---\n你是子代理",
    );
    let paths = make_agent_paths(home.path().to_path_buf(), None, Vec::new());

    let def = load_agent_definition_from_agent_paths(&paths, "sub");

    assert_eq!(def.mode, AgentMode::Subagent);
}

#[test]
fn load_definition_ignores_agent_id() {
    // agent_id 与 definition 正交：定义加载由 name 参数决定
    let home = temp_home();
    let paths = fuyao_api::AgentPaths {
        agent_id: Some("global/coder".to_string()),
        workspace: None,
        extra_dirs: Vec::new(),
        fuyao_home: home.path().to_path_buf(),
    };

    let def = load_agent_definition_from_agent_paths(&paths, "default");

    // 无 default.md → 回退内置，与 agent_id 无关
    assert_eq!(def.name, "fuyao");
}

#[test]
fn load_definition_empty_frontmatter_still_loads_body() {
    // 空 frontmatter（---\n---）不应导致解析失败，body 仍被加载
    let home = temp_home();
    write_default_agent(home.path(), "---\n---\n仅有正文无元数据");
    let paths = make_agent_paths(home.path().to_path_buf(), None, Vec::new());

    let def = load_agent_definition_from_agent_paths(&paths, "default");

    assert!(
        def.system_prompt.contains("仅有正文"),
        "空 frontmatter 下正文应保留"
    );
    assert!(def.name.is_empty(), "空 frontmatter 下 name 为空串");
}
