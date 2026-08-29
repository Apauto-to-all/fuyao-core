//! 配置系统集成测试
//!
//! 钉死 `load_merged_config` / `load_config` 这两个公开 API 的契约：
//! - 三层合并语义（workspace > agent > global）与深合并字段级保留
//! - 文件不存在的静默跳过（FileNotFound 变体从不返回）
//! - `${VAR}` 环境变量插值（仅 mcp_servers 段）
//! - 错误路径（TOML 解析失败、未知字段）
//!
//! 与源文件内单元测试的分工：单元测试覆盖单 section 的 serde 默认值；
//! 本文件覆盖跨函数的加载/合并/插值契约。

mod common;

use fuyao_api::{ConfigError, LogRotation, load_config, load_merged_config};
use rstest::rstest;

// ---------------------------------------------------------------------------
// load_merged_config：空输入与文件不存在的契约
// ---------------------------------------------------------------------------

#[test]
fn load_merged_config_all_none_returns_ok_none() {
    // 三层全 None → Ok(None)（钉死：不是 Err）
    let result = load_merged_config(None, None, None).unwrap();
    assert!(result.is_none());
}

#[test]
fn load_merged_config_nonexistent_path_is_silently_skipped() {
    // 文件路径 Some 但实际不存在 → 跳过，返回 Ok(None)
    // 重要契约：FileNotFound 变体定义了但从不从此路径返回
    let result = load_merged_config(
        Some(std::path::Path::new("/nonexistent_global_12345/fuyao.toml")),
        None,
        None,
    )
    .unwrap();
    assert!(result.is_none(), "不存在的路径应被静默跳过");
}

#[test]
fn load_merged_config_empty_toml_file_is_treated_as_no_config() {
    let temp = tempfile::tempdir().expect("创建临时目录失败");
    common::write_config_file(temp.path(), "");

    // 空文件 → table.is_empty() → 不计入 has_any_config → Ok(None)
    let path = temp.path().join("fuyao.toml");
    let result = load_merged_config(Some(&path), None, None).unwrap();
    assert!(result.is_none(), "空 TOML 文件应视为无配置");
}

// ---------------------------------------------------------------------------
// load_merged_config：单层加载基础契约
// ---------------------------------------------------------------------------

#[test]
fn load_merged_config_single_layer_populates_section() {
    let temp = tempfile::tempdir().expect("创建临时目录失败");
    common::write_config_file(
        temp.path(),
        r#"
[llm]
request_timeout_secs = 600
"#,
    );
    let path = temp.path().join("fuyao.toml");

    let config = load_merged_config(Some(&path), None, None)
        .unwrap()
        .expect("应加载到配置");
    assert_eq!(config.llm.request_timeout_secs, 600);
    // 缺省字段走默认
    assert_eq!(config.llm.connect_timeout_secs, 10);
    assert_eq!(config.llm.retry.initial_delay_ms, 2000);
}

#[test]
fn load_merged_config_returns_default_for_absent_sections() {
    // 只配 [llm]，其余 section 应全部走默认值（#[serde(default)] 契约）
    let temp = tempfile::tempdir().expect("创建临时目录失败");
    common::write_config_file(
        temp.path(),
        r#"
[llm]
request_timeout_secs = 100
"#,
    );
    let path = temp.path().join("fuyao.toml");
    let config = load_merged_config(Some(&path), None, None)
        .unwrap()
        .expect("应有配置");

    // 未配置的 section 走各自 Default
    assert_eq!(config.logging.level, "info");
    assert!(config.logging.console);
    assert_eq!(config.logging.rotation, LogRotation::Daily);
    assert_eq!(config.engine.inbound_channel_capacity, 32);
    assert_eq!(config.hooks.timeout_secs, 5);
}

// ---------------------------------------------------------------------------
// 三层优先级：workspace > agent > global
// ---------------------------------------------------------------------------

#[test]
fn three_layer_precedence_workspace_overrides_global() {
    let temp = tempfile::tempdir().expect("创建临时目录失败");
    let global_dir = temp.path().join("global");
    let agent_dir = temp.path().join("agent");
    let ws_dir = temp.path().join("workspace");
    std::fs::create_dir_all(&global_dir).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&ws_dir).unwrap();

    common::write_config_file(
        &global_dir,
        r#"
[llm]
request_timeout_secs = 100
"#,
    );
    common::write_config_file(
        &agent_dir,
        r#"
[llm]
request_timeout_secs = 200
"#,
    );
    common::write_config_file(
        &ws_dir,
        r#"
[llm]
request_timeout_secs = 300
"#,
    );

    let config = load_merged_config(
        Some(&global_dir.join("fuyao.toml")),
        Some(&agent_dir.join("fuyao.toml")),
        Some(&ws_dir.join("fuyao.toml")),
    )
    .unwrap()
    .expect("应加载到配置");

    // workspace 层最高优先级覆盖
    assert_eq!(config.llm.request_timeout_secs, 300);
    // global 层的 llm.request_timeout_secs 被保留（其他层未覆盖）
}

#[test]
fn agent_layer_overrides_global() {
    let temp = tempfile::tempdir().expect("创建临时目录失败");
    let global_dir = temp.path().join("g");
    let agent_dir = temp.path().join("a");
    std::fs::create_dir_all(&global_dir).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();

    common::write_config_file(
        &global_dir,
        r#"[llm]\nrequest_timeout_secs = 100"#.replace("\\n", "\n").as_str(),
    );
    common::write_config_file(
        &agent_dir,
        r#"[llm]\nrequest_timeout_secs = 200"#.replace("\\n", "\n").as_str(),
    );

    let config = load_merged_config(
        Some(&global_dir.join("fuyao.toml")),
        Some(&agent_dir.join("fuyao.toml")),
        None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(config.llm.request_timeout_secs, 200, "agent 应覆盖 global");
}

// ---------------------------------------------------------------------------
// 深合并：字段级保留，子段不整体替换
// ---------------------------------------------------------------------------

#[test]
fn deep_merge_preserves_fields_from_lower_layer() {
    let temp = tempfile::tempdir().expect("创建临时目录失败");
    let global_dir = temp.path().join("g");
    let ws_dir = temp.path().join("w");
    std::fs::create_dir_all(&global_dir).unwrap();
    std::fs::create_dir_all(&ws_dir).unwrap();

    // global 配 [llm.retry] 的 initial_delay_ms
    common::write_config_file(
        &global_dir,
        r#"
[llm.retry]
initial_delay_ms = 500
"#,
    );
    // workspace 只配 [llm.retry] 的 max_delay_ms
    common::write_config_file(
        &ws_dir,
        r#"
[llm.retry]
max_delay_ms = 999
"#,
    );

    let config = load_merged_config(
        Some(&global_dir.join("fuyao.toml")),
        None,
        Some(&ws_dir.join("fuyao.toml")),
    )
    .unwrap()
    .unwrap();

    // 两个字段都应存在（深合并不丢字段）
    assert_eq!(config.llm.retry.initial_delay_ms, 500, "来自 global 层");
    assert_eq!(config.llm.retry.max_delay_ms, 999, "来自 workspace 层");
}

// ---------------------------------------------------------------------------
// ${VAR} 环境变量插值（仅 mcp_servers 段）
// ---------------------------------------------------------------------------

#[test]
fn env_var_interpolation_replaces_existing_var_in_mcp_servers() {
    // 用进程唯一前缀避免并行测试串扰
    let var_name = format!("FUYAO_TEST_CFG_EXIST_{}_{}", std::process::id(), "interp1");
    // SAFETY: 单线程测试上下文，var_name 进程唯一，不会与其他测试冲突
    unsafe { std::env::set_var(&var_name, "resolved_value") };

    let temp = tempfile::tempdir().expect("创建临时目录失败");
    let toml_content = format!(
        r#"
[mcp_servers.test]
command = "${{{var_name}}}"
"#,
    );
    common::write_config_file(temp.path(), &toml_content);
    let path = temp.path().join("fuyao.toml");

    let config = load_merged_config(Some(&path), None, None)
        .unwrap()
        .unwrap();
    // SAFETY: 同上，单线程清理进程唯一变量
    unsafe { std::env::remove_var(&var_name) };

    let cmd = config
        .mcp_servers
        .get("test")
        .expect("应有 test server")
        .command
        .as_deref()
        .expect("应有 command");
    assert_eq!(cmd, "resolved_value");
}

#[test]
fn env_var_interpolation_keeps_literal_when_var_missing() {
    let temp = tempfile::tempdir().expect("创建临时目录失败");
    common::write_config_file(
        temp.path(),
        r#"
[mcp_servers.test]
command = "${FUYAO_TEST_CFG_NEVER_EXISTS_98765}"
"#,
    );
    let path = temp.path().join("fuyao.toml");

    let config = load_merged_config(Some(&path), None, None)
        .unwrap()
        .unwrap();
    let cmd = config
        .mcp_servers
        .get("test")
        .unwrap()
        .command
        .as_deref()
        .unwrap();
    // 变量不存在 → 保留字面量
    assert_eq!(cmd, "${FUYAO_TEST_CFG_NEVER_EXISTS_98765}");
}

#[test]
fn env_var_interpolation_unclosed_brace_kept_as_literal() {
    let temp = tempfile::tempdir().expect("创建临时目录失败");
    common::write_config_file(
        temp.path(),
        r#"
[mcp_servers.test]
command = "echo ${UNCLOSED"
"#,
    );
    let path = temp.path().join("fuyao.toml");

    let config = load_merged_config(Some(&path), None, None)
        .unwrap()
        .unwrap();
    let cmd = config
        .mcp_servers
        .get("test")
        .unwrap()
        .command
        .as_deref()
        .unwrap();
    // 无闭合 } → 剩余整体作为字面量
    assert_eq!(cmd, "echo ${UNCLOSED");
}

#[test]
fn env_var_interpolation_not_applied_outside_mcp_servers() {
    // 非 mcp_servers 段的 ${VAR} 不应被插值
    let var_name = "FUYAO_TEST_CFG_OUTSIDE_11111";
    // SAFETY: 单线程测试上下文，var_name 唯一
    unsafe { std::env::set_var(var_name, "should_not_appear") };

    let temp = tempfile::tempdir().expect("创建临时目录失败");
    let toml_content = format!(
        r#"
[logging]
level = "${{{var_name}}}"
"#,
    );
    common::write_config_file(temp.path(), &toml_content);
    let path = temp.path().join("fuyao.toml");

    let config = load_merged_config(Some(&path), None, None)
        .unwrap()
        .unwrap();
    // SAFETY: 同上
    unsafe { std::env::remove_var(var_name) };

    // logging 段不做插值，保留字面量
    assert_eq!(config.logging.level, format!("${{{var_name}}}"));
}

// ---------------------------------------------------------------------------
// 错误路径：TOML 解析失败、未知字段
// ---------------------------------------------------------------------------

#[test]
fn invalid_toml_returns_toml_error_variant() {
    let temp = tempfile::tempdir().expect("创建临时目录失败");
    common::write_config_file(temp.path(), "this is = = not valid toml {{{");
    let path = temp.path().join("fuyao.toml");

    let result = load_merged_config(Some(&path), None, None);
    assert!(result.is_err());
    // 钉死错误变体：应是 TomlError，而非 IoError / FileNotFound
    assert!(
        matches!(result.unwrap_err(), ConfigError::TomlError(_)),
        "无效 TOML 应返回 TomlError 变体"
    );
}

#[test]
fn models_unknown_tag_returns_error() {
    // ModelSelection deny_unknown_fields：未知标签 [models.vision] → Err
    let temp = tempfile::tempdir().expect("创建临时目录失败");
    common::write_config_file(
        temp.path(),
        r#"
[models.vision]
model = "some/vision-model"
"#,
    );
    let path = temp.path().join("fuyao.toml");

    let result = load_merged_config(Some(&path), None, None);
    assert!(result.is_err(), "未知 models 标签应报错");
}

#[rstest]
fn log_rotation_invalid_value_fails(#[values("weekly", "yearly", "DAILY", "Daily")] bad: &str) {
    // LogRotation 仅接受 daily/hourly/never（小写），其余应解析失败
    let temp = tempfile::tempdir().expect("创建临时目录失败");
    let content = format!("[logging]\nrotation = \"{bad}\"\n");
    common::write_config_file(temp.path(), &content);
    let path = temp.path().join("fuyao.toml");

    let result = load_merged_config(Some(&path), None, None);
    assert!(result.is_err(), "非法 rotation 值 '{bad}' 应报错");
}

// ---------------------------------------------------------------------------
// load_config：经 AgentPaths 端到端
// ---------------------------------------------------------------------------

#[test]
fn load_config_via_agent_paths_merges_global_and_workspace() {
    // 端到端验证 load_config 经 AgentPaths 合并 global 与 workspace 两层。
    // 注：agent 层路径经 get_agent_root（全局函数，读环境变量），其解析契约
    // 由 paths_test 覆盖；此处聚焦由 fuyao_home/workspace 字段可控的两层。
    let temp = tempfile::tempdir().expect("创建临时目录失败");
    let home = temp.path().join("home");
    let ws = temp.path().join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(ws.join(".fuyao")).unwrap();

    // global: fuyao_home/fuyao.toml
    common::write_config_file(
        &home,
        r#"
[llm]
request_timeout_secs = 100
[llm.retry]
initial_delay_ms = 500
"#,
    );
    // workspace: ws/.fuyao/fuyao.toml
    common::write_config_file(
        &ws.join(".fuyao"),
        r#"
[llm]
request_timeout_secs = 999
"#,
    );

    let paths = common::make_agent_paths(home, None, Some(ws));
    let config = load_config(&paths).unwrap().expect("应加载到配置");

    // workspace 覆盖 request_timeout_secs
    assert_eq!(config.llm.request_timeout_secs, 999);
    // global 层 [llm.retry].initial_delay_ms 经深合并保留
    assert_eq!(config.llm.retry.initial_delay_ms, 500);
}

#[test]
fn load_config_returns_none_when_no_config_anywhere() {
    let temp = tempfile::tempdir().expect("创建临时目录失败");
    let paths = common::make_agent_paths(
        temp.path().to_path_buf(),
        Some("global/nonexistent_agent_xyz"),
        None,
    );
    // 没有任何配置文件存在
    let result = load_config(&paths).unwrap();
    assert!(result.is_none());
}

// ---------------------------------------------------------------------------
// ModelSelection 固定标签契约
// ---------------------------------------------------------------------------

#[test]
fn model_selection_accepts_fast_tag() {
    let temp = tempfile::tempdir().expect("创建临时目录失败");
    common::write_config_file(
        temp.path(),
        r#"
[models.fast]
model = "deepseek/deepseek-v4-flash"
"#,
    );
    let path = temp.path().join("fuyao.toml");

    let config = load_merged_config(Some(&path), None, None)
        .unwrap()
        .unwrap();
    assert_eq!(
        config.models.fast.as_ref().unwrap().model,
        "deepseek/deepseek-v4-flash"
    );
}
