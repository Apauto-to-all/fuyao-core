//! 路径系统集成测试
//!
//! 钉死 `AgentPaths` 与路径解析函数的公共 API 契约：
//! - agent_id 三形式（global/{名} / workspace/{名} / 裸名）× workspace 有无 的解析组合
//! - 各纯函数路径方法（config_paths / env_paths / sessions_db_path / logs_dir / cache_key 等）
//! - extra_dirs 过滤分支（skills_paths / instructions_paths / agents_def_paths）
//!
//! 关键策略：`AgentPaths` 是纯数据结构体，`fuyao_home` 字段可直接注入，
//! 实现零环境变量依赖的 per-test 隔离（呼应路径系统的「纯函数」设计）。

mod common;

use std::path::PathBuf;

use fuyao_api::AgentPaths;
use rstest::rstest;

// ---------------------------------------------------------------------------
// get_agent_root：agent_id 三形式 × workspace 有无（核心解析规则）
// ---------------------------------------------------------------------------

/// `global/{名}`：强制全局层，忽略 workspace 参数，结果落在 fuyao-agents 下
#[test]
fn get_agent_root_global_prefix_ignores_workspace() {
    let root = fuyao_api::get_agent_root("global/coder", None);
    assert!(
        root.to_string_lossy().contains("fuyao-agents"),
        "应在 fuyao-agents 下"
    );
    assert!(root.to_string_lossy().ends_with("coder"), "应以 name 结尾");
}

/// `workspace/{名}` + workspace=Some：强制工作目录层 `{ws}/.fuyao/fuyao-agents/{名}`
#[test]
fn get_agent_root_workspace_prefix_with_workspace() {
    let ws = PathBuf::from("/tmp/project");
    let root = fuyao_api::get_agent_root("workspace/coder", Some(&ws));
    // 平台无关断言：按路径组件逐段检查（Windows 用 \，Unix 用 /）
    let expected = get_fuyao_workspace_agents_dir(&ws).join("coder");
    assert_eq!(root, expected);
}

/// 构造工作目录 agents 目录路径（与源码 get_workspace_agents_dir 一致的拼接逻辑）
fn get_fuyao_workspace_agents_dir(ws: &std::path::Path) -> PathBuf {
    ws.join(".fuyao").join("fuyao-agents")
}

/// `workspace/{名}` + workspace=None：走 resolve 回退，整串当名字落全局层（微妙契约）
#[test]
fn get_agent_root_workspace_prefix_without_workspace_falls_back() {
    // workspace 前缀但无 workspace 参数 → 进入 resolve_agent_root("workspace/coder", None)
    // → 无 workspace 可检查 → 整串 "workspace/coder" 作为 agent_id 落全局层
    let root = fuyao_api::get_agent_root("workspace/coder", None);
    assert!(
        root.to_string_lossy().contains("fuyao-agents"),
        "应回退到全局层"
    );
    assert!(
        root.to_string_lossy().ends_with("workspace/coder"),
        "整串（含斜杠）应作为名字"
    );
}

/// 裸名 + 无 workspace：落全局层
#[test]
fn get_agent_root_bare_id_without_workspace_falls_to_global() {
    let root = fuyao_api::get_agent_root("nonexistent_xyz", None);
    assert!(
        root.to_string_lossy().contains("fuyao-agents"),
        "裸名应落全局层"
    );
    assert!(root.to_string_lossy().ends_with("nonexistent_xyz"));
}

/// 裸名 + workspace 存在目录：优先 workspace-local（需真实文件系统）
#[test]
fn get_agent_root_bare_id_prefers_existing_workspace_local() {
    let temp = tempfile::tempdir().expect("创建临时目录失败");
    let ws = temp.path();
    // 预创建 workspace-local 的 agent 目录
    let local = ws.join(".fuyao").join("fuyao-agents").join("coder");
    std::fs::create_dir_all(&local).unwrap();

    let root = fuyao_api::get_agent_root("coder", Some(ws));
    assert_eq!(root, local, "存在的 workspace-local 目录应优先");
}

/// 裸名 + workspace 但目录不存在：回退全局层
#[test]
fn get_agent_root_bare_id_falls_back_when_workspace_local_missing() {
    let temp = tempfile::tempdir().expect("创建临时目录失败");
    let ws = temp.path();
    // 不创建 workspace-local 目录
    let root = fuyao_api::get_agent_root("coder", Some(ws));
    assert!(
        root.to_string_lossy().contains("fuyao-agents"),
        "workspace-local 不存在时回退全局层"
    );
    assert!(root.to_string_lossy().ends_with("coder"));
}

// ---------------------------------------------------------------------------
// AgentPaths 纯函数方法：注入 fuyao_home，零环境变量
// ---------------------------------------------------------------------------

#[test]
fn config_paths_uses_injected_home() {
    let home = PathBuf::from("/tmp/injected_home");
    let paths = common::make_agent_paths(home.clone(), None, None);

    let cp = paths.config_paths();
    assert_eq!(cp.global_, Some(home.join("fuyao.toml")));
    // 无 agent_id → agent 层 None
    assert!(cp.agent.is_none());
    // 无 workspace → workspace 层 None
    assert!(cp.workspace.is_none());
}

#[test]
fn config_paths_three_layers_with_agent_id_and_workspace() {
    let home = PathBuf::from("/tmp/home");
    let ws = PathBuf::from("/tmp/project");
    let paths = common::make_agent_paths(home.clone(), Some("global/coder"), Some(ws.clone()));

    let cp = paths.config_paths();
    assert_eq!(cp.global_, Some(home.join("fuyao.toml")));
    assert_eq!(cp.workspace, Some(ws.join(".fuyao").join("fuyao.toml")));
    // agent 层 = agent_root/fuyao.toml（global/coder 的 agent_root 落全局 agents 目录）
    let agent = cp.agent.expect("有 agent_id 时 agent 层应为 Some");
    assert!(agent.ends_with("fuyao.toml"));
}

#[test]
fn env_paths_uses_injected_home() {
    let home = PathBuf::from("/tmp/home");
    let paths = common::make_agent_paths(home.clone(), None, None);

    let ep = paths.env_paths();
    assert_eq!(ep.global_, Some(home.join(".env")));
}

/// sessions_db_path：有 agent_id → agent 层；无 agent_id → 全局层；永不 panic
#[rstest]
fn sessions_db_path_never_panics(#[values(None, Some("global/coder"))] agent_id: Option<&str>) {
    let home = PathBuf::from("/tmp/home");
    let paths = common::make_agent_paths(home, agent_id, None);

    // 不论 agent_id 有无，都不应 panic 且返回有效路径
    let db = paths.sessions_db_path();
    assert!(db.to_string_lossy().ends_with("sessions.db"));
}

#[test]
fn sessions_db_paths_global_only_when_no_agent_id() {
    let home = PathBuf::from("/tmp/home");
    let paths = common::make_agent_paths(home.clone(), None, None);

    let sdp = paths.sessions_db_paths();
    assert_eq!(sdp.global_, Some(home.join("sessions").join("sessions.db")));
    assert!(sdp.agent.is_none());
    assert!(sdp.workspace.is_none(), "sessions.db 不走 workspace 层");
}

#[test]
fn sessions_db_paths_agent_only_when_has_agent_id() {
    let home = PathBuf::from("/tmp/home");
    let paths = common::make_agent_paths(home, Some("global/coder"), None);

    let sdp = paths.sessions_db_paths();
    // 有 agent_id → global_ 为 None
    assert!(sdp.global_.is_none());
    assert!(sdp.agent.is_some());
}

/// logs_dir：有 agent_id → agent 层；无 → 全局层
#[test]
fn logs_dir_uses_global_when_no_agent_id() {
    let home = PathBuf::from("/tmp/home");
    let paths = common::make_agent_paths(home.clone(), None, None);
    assert_eq!(paths.logs_dir(), home.join("logs"));
}

#[test]
fn logs_dir_uses_agent_root_when_has_agent_id() {
    let home = PathBuf::from("/tmp/home");
    let paths = common::make_agent_paths(home, Some("global/coder"), None);
    let logs = paths.logs_dir();
    assert!(logs.ends_with("logs"));
    // 落在 agent_root 下而非 home 根
    let agent_root = paths.agent_root().expect("应有 agent_root");
    assert!(logs.starts_with(&agent_root));
}

// ---------------------------------------------------------------------------
// cache_key：格式契约
// ---------------------------------------------------------------------------

#[test]
fn cache_key_format_with_agent_and_workspace() {
    let paths = common::make_agent_paths(
        PathBuf::from("/tmp/h"),
        Some("global/coder"),
        Some(PathBuf::from("/tmp/project")),
    );
    assert_eq!(paths.cache_key(), "global/coder|/tmp/project");
}

#[test]
fn cache_key_default_is_pipe_only() {
    let paths = AgentPaths::default();
    assert_eq!(paths.cache_key(), "|");
}

// ---------------------------------------------------------------------------
// agents_md_paths：workspace 层是 ws/AGENTS.md（不带 .fuyao/）
// ---------------------------------------------------------------------------

#[test]
fn agents_md_paths_workspace_layer_omits_fuyao_prefix() {
    let ws = PathBuf::from("/tmp/project");
    let paths = common::make_agent_paths(PathBuf::from("/tmp/h"), None, Some(ws.clone()));

    let amp = paths.agents_md_paths();
    // workspace 层应是 ws/AGENTS.md，而非 ws/.fuyao/AGENTS.md
    assert_eq!(amp.workspace, Some(ws.join("AGENTS.md")));
    assert_ne!(amp.workspace, Some(ws.join(".fuyao").join("AGENTS.md")));
}

// ---------------------------------------------------------------------------
// extra_dirs 过滤分支（需真实文件系统）
// ---------------------------------------------------------------------------

#[test]
fn skills_paths_filters_nonexistent_extra_dirs() {
    let temp = tempfile::tempdir().expect("创建临时目录失败");
    let plugin_a = temp.path().join("plugin-a");
    let plugin_b = temp.path().join("plugin-b");
    // plugin-a 有 skills 子目录
    std::fs::create_dir_all(plugin_a.join("skills")).unwrap();
    // plugin-b 没有 skills 子目录

    let mut paths = common::make_agent_paths(PathBuf::from("/tmp/h"), None, None);
    paths.extra_dirs = vec![plugin_a.clone(), plugin_b];

    let sp = paths.skills_paths();
    assert_eq!(sp.extra.len(), 1, "只保留存在 skills/ 的插件目录");
    assert!(sp.extra[0].ends_with("skills"));
    assert!(sp.extra[0].starts_with(&plugin_a));
}

#[test]
fn instructions_paths_filters_nonexistent_extra_dirs() {
    let temp = tempfile::tempdir().expect("创建临时目录失败");
    let plugin_a = temp.path().join("plugin-a");
    let plugin_b = temp.path().join("plugin-b");
    std::fs::create_dir_all(plugin_a.join("instructions")).unwrap();
    // plugin-b 没有 instructions/

    let mut paths = common::make_agent_paths(PathBuf::from("/tmp/h"), None, None);
    paths.extra_dirs = vec![plugin_a.clone(), plugin_b];

    let ip = paths.instructions_paths();
    assert_eq!(ip.extra.len(), 1);
    assert!(ip.extra[0].starts_with(&plugin_a));
}

#[test]
fn agents_def_paths_filters_extra_by_existing_file() {
    let temp = tempfile::tempdir().expect("创建临时目录失败");
    let plugin_a = temp.path().join("plugin-a");
    let plugin_b = temp.path().join("plugin-b");
    // plugin-a 有 agents/default.md
    std::fs::create_dir_all(plugin_a.join("agents")).unwrap();
    std::fs::write(plugin_a.join("agents").join("default.md"), "# A").unwrap();
    // plugin-b 有 agents/ 但无 default.md
    std::fs::create_dir_all(plugin_b.join("agents")).unwrap();

    let mut paths = common::make_agent_paths(PathBuf::from("/tmp/h"), None, None);
    paths.extra_dirs = vec![plugin_a.clone(), plugin_b];

    let ap = paths.agents_def_paths("default");
    assert_eq!(ap.extra.len(), 1, "只保留存在 default.md 的插件");
    assert!(ap.extra[0].starts_with(&plugin_a));
}

// ---------------------------------------------------------------------------
// LayeredPaths：all() 优先级顺序与 first_exists / merge_exists
// ---------------------------------------------------------------------------

#[test]
fn layered_paths_all_orders_by_priority_descending() {
    let home = PathBuf::from("/tmp/home");
    let ws = PathBuf::from("/tmp/project");
    let paths = common::make_agent_paths(home.clone(), Some("global/coder"), Some(ws.clone()));

    let cp = paths.config_paths();
    let all = cp.all();
    // all() 顺序：workspace → agent → global_ → extra（优先级降序）
    assert_eq!(all.len(), 3, "三层配置路径都应存在");
    // 用精确路径比较，避免平台分隔符问题
    assert_eq!(
        all[0],
        ws.join(".fuyao").join("fuyao.toml"),
        "第一项应为 workspace 层"
    );
    assert_eq!(all[2], home.join("fuyao.toml"), "最后一项应为 global 层");
}

#[test]
fn layered_paths_first_exists_returns_existing_file() {
    let temp = tempfile::tempdir().expect("创建临时目录失败");
    // 只在 global 位置创建文件
    let global_file = temp.path().join("fuyao.toml");
    std::fs::write(&global_file, "").unwrap();

    let paths = common::make_agent_paths(temp.path().to_path_buf(), None, None);
    let cp = paths.config_paths();
    let first = cp.first_exists();
    assert_eq!(first, Some(global_file));
}

#[test]
fn layered_paths_merge_exists_collects_all_existing() {
    let temp = tempfile::tempdir().expect("创建临时目录失败");
    // global 与 workspace 位置都创建文件
    let global_file = temp.path().join("fuyao.toml");
    std::fs::write(&global_file, "").unwrap();
    let ws = temp.path().join("project");
    std::fs::create_dir_all(ws.join(".fuyao")).unwrap();
    let ws_file = ws.join(".fuyao").join("fuyao.toml");
    std::fs::write(&ws_file, "").unwrap();

    let paths = common::make_agent_paths(temp.path().to_path_buf(), None, Some(ws));
    let cp = paths.config_paths();
    let merged = cp.merge_exists();
    assert_eq!(merged.len(), 2, "应合并所有存在的文件");
}
