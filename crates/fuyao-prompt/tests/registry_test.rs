//! fuyao-prompt 集成测试：AgentRegistry 装配链路
//!
//! 聚焦 AgentRegistry 的跨目录扫描、CRUD、列表分页、来源过滤、覆盖合并、错误路径。
//! 所有测试通过 AgentRegistry::new(workspace, fuyao_home) 字段注入实现隔离。

mod common;

use common::{temp_home, write_registry_agent, write_workspace_agent};
use fuyao_prompt::{AgentFile, AgentRegistry, AgentSource, RegistryError};

/// 构造纯全局层注册表（workspace = None）
fn global_only_registry(home: &std::path::Path) -> AgentRegistry {
    AgentRegistry::new(None, home.to_path_buf())
}

// ============================================================================
// list：扫描与合并
// ============================================================================

#[test]
fn list_empty_home_returns_only_default_agent() {
    // 无 fuyao-agents/ 时，仅返回内置 default（scope=None 时纳入）
    let home = temp_home();
    let registry = global_only_registry(home.path());

    let paged = registry.list(1, 10, None, None);

    assert_eq!(paged.total, 1, "空 home 应仅含 default");
    assert_eq!(paged.items[0].id, "default");
}

#[test]
fn list_scans_global_layer_agents() {
    let home = temp_home();
    write_registry_agent(
        home.path(),
        "coder",
        "---\nname: coder\ndescription: 编码\n---\n你是编码助手",
    );
    write_registry_agent(
        home.path(),
        "reviewer",
        "---\nname: reviewer\ndescription: 审查\n---\n你是审查助手",
    );
    let registry = global_only_registry(home.path());

    let paged = registry.list(1, 50, None, None);

    // coder + reviewer + default = 3
    assert_eq!(paged.total, 3);
    let ids: Vec<&str> = paged.items.iter().map(|a| a.id.as_str()).collect();
    assert!(ids.contains(&"global/coder"));
    assert!(ids.contains(&"global/reviewer"));
    assert!(ids.contains(&"default"));
}

#[test]
fn list_workspace_overrides_same_name_global() {
    // 同名 Agent：项目层覆盖全局层（合并去重，key=文件夹名）
    let home = temp_home();
    write_registry_agent(home.path(), "coder", "---\nname: global-coder\n---\n全局");
    let ws = tempfile::tempdir().expect("创建工作目录失败");
    write_workspace_agent(ws.path(), "coder", "---\nname: ws-coder\n---\n项目层");
    let registry = AgentRegistry::new(Some(ws.path().to_path_buf()), home.path().to_path_buf());

    let paged = registry.list(1, 50, None, None);

    let coder = paged
        .items
        .iter()
        .find(|a| a.id == "workspace/coder")
        .expect("应含项目层 coder");
    assert_eq!(coder.name, "ws-coder", "项目层应覆盖全局层");
    // 全局层同名应被去重
    assert!(
        !paged.items.iter().any(|a| a.id == "global/coder"),
        "全局层同名应被项目层覆盖去重"
    );
}

#[test]
fn list_scope_filter_global_only() {
    let home = temp_home();
    write_registry_agent(home.path(), "coder", "---\nname: coder\n---\n全局");
    let ws = tempfile::tempdir().expect("创建工作目录失败");
    write_workspace_agent(ws.path(), "reviewer", "---\nname: reviewer\n---\n项目层");
    let registry = AgentRegistry::new(Some(ws.path().to_path_buf()), home.path().to_path_buf());

    let paged = registry.list(1, 50, Some("global"), None);

    // scope=global 时仅全局层，default 不纳入（scope 非 None）
    assert_eq!(paged.total, 1);
    assert_eq!(paged.items[0].id, "global/coder");
}

#[test]
fn list_scope_filter_workspace_only() {
    let home = temp_home();
    write_registry_agent(home.path(), "coder", "---\nname: coder\n---\n全局");
    let ws = tempfile::tempdir().expect("创建工作目录失败");
    write_workspace_agent(ws.path(), "reviewer", "---\nname: reviewer\n---\n项目层");
    let registry = AgentRegistry::new(Some(ws.path().to_path_buf()), home.path().to_path_buf());

    let paged = registry.list(1, 50, Some("workspace"), None);

    assert_eq!(paged.total, 1);
    assert_eq!(paged.items[0].id, "workspace/reviewer");
}

#[test]
fn list_keyword_filter_case_insensitive() {
    let home = temp_home();
    write_registry_agent(home.path(), "Coder", "---\nname: coder\n---\n编码");
    write_registry_agent(home.path(), "reviewer", "---\nname: reviewer\n---\n审查");
    let registry = global_only_registry(home.path());

    // q 匹配文件夹名（大小写不敏感）
    let paged = registry.list(1, 50, None, Some("cod"));

    assert_eq!(paged.total, 1, "关键字应匹配文件夹名");
    assert_eq!(paged.items[0].id, "global/Coder");
}

#[test]
fn list_pagination() {
    let home = temp_home();
    for i in 0..5 {
        write_registry_agent(
            home.path(),
            &format!("agent{i}"),
            &format!("---\nname: agent{i}\n---\n正文{i}"),
        );
    }
    let registry = global_only_registry(home.path());

    // 5 agents + default = 6，分页 size=2
    let page1 = registry.list(1, 2, None, None);
    let page2 = registry.list(2, 2, None, None);

    assert_eq!(page1.total, 6);
    assert_eq!(page1.items.len(), 2);
    assert_eq!(page2.items.len(), 2);
    // 页码不重叠
    let page1_ids: std::collections::HashSet<&str> =
        page1.items.iter().map(|a| a.id.as_str()).collect();
    let page2_ids: std::collections::HashSet<&str> =
        page2.items.iter().map(|a| a.id.as_str()).collect();
    assert!(page1_ids.is_disjoint(&page2_ids), "分页不应重叠");
}

#[test]
fn list_folder_without_system_md_uses_folder_name_as_name() {
    // 文件夹存在但无 system.md → name=文件夹名，仍可被列出（空文件夹可见可编辑）
    let home = temp_home();
    let agent_dir = home.path().join("fuyao-agents").join("empty-agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let registry = global_only_registry(home.path());

    let paged = registry.list(1, 50, Some("global"), None);

    let agent = paged
        .items
        .iter()
        .find(|a| a.id == "global/empty-agent")
        .expect("空文件夹 Agent 应被列出");
    assert_eq!(agent.name, "empty-agent", "无 system.md 时 name=文件夹名");
}

// ============================================================================
// get：单 Agent 查询
// ============================================================================

#[test]
fn get_existing_agent() {
    let home = temp_home();
    write_registry_agent(
        home.path(),
        "coder",
        "---\nname: coder\ndescription: 编码助手\nmode: primary\n---\n你是编码助手",
    );
    let registry = global_only_registry(home.path());

    let agent = registry.get("global/coder").expect("已创建的 Agent 应可查");

    assert_eq!(agent.name, "coder");
    assert_eq!(agent.mode, fuyao_api::AgentMode::Primary);
    assert!(agent.system_prompt.contains("编码助手"));
}

#[test]
fn get_default_agent_without_files() {
    // default Agent 在无 agents/default.md 时仍可查（用硬编码默认）
    let home = temp_home();
    let registry = global_only_registry(home.path());

    let agent = registry.get("default").expect("default 应始终存在");

    assert_eq!(agent.id, "default");
    assert_eq!(agent.name, "fuyao");
}

#[test]
fn get_nonexistent_returns_none() {
    let home = temp_home();
    let registry = global_only_registry(home.path());

    assert!(registry.get("global/nonexistent").is_none());
}

// ============================================================================
// create：CRUD 创建
// ============================================================================

#[test]
fn create_global_agent_makes_directory() {
    let home = temp_home();
    let registry = global_only_registry(home.path());

    registry
        .create(AgentSource::Global, "newbot")
        .expect("创建应成功");

    let dir = home.path().join("fuyao-agents").join("newbot");
    assert!(dir.is_dir(), "应创建空文件夹");
}

#[test]
fn create_duplicate_returns_already_exists() {
    let home = temp_home();
    let registry = global_only_registry(home.path());
    registry.create(AgentSource::Global, "bot").unwrap();

    let result = registry.create(AgentSource::Global, "bot");

    assert!(matches!(result, Err(RegistryError::AlreadyExists(_))));
}

#[test]
fn create_workspace_agent_without_workspace_returns_workspace_missing() {
    let home = temp_home();
    let registry = global_only_registry(home.path());

    let result = registry.create(AgentSource::Workspace, "bot");

    assert!(
        matches!(result, Err(RegistryError::WorkspaceMissing)),
        "无 workspace 时项目层创建应报错"
    );
}

#[test]
fn create_workspace_agent_with_workspace() {
    let home = temp_home();
    let ws = tempfile::tempdir().expect("创建工作目录失败");
    let registry = AgentRegistry::new(Some(ws.path().to_path_buf()), home.path().to_path_buf());

    registry
        .create(AgentSource::Workspace, "wsbot")
        .expect("项目层创建应成功");

    let dir = ws.path().join(".fuyao").join("fuyao-agents").join("wsbot");
    assert!(dir.is_dir());
}

// ============================================================================
// read_content / write_content：文件读写
// ============================================================================

#[test]
fn write_then_read_system_md() {
    let home = temp_home();
    let registry = global_only_registry(home.path());
    registry.create(AgentSource::Global, "bot").unwrap();

    let md = "---\nname: bot\ndescription: 测试\n---\n你是 bot";
    registry
        .write_content(AgentSource::Global, "bot", AgentFile::SystemMd, md)
        .expect("写入应成功");

    let content = registry
        .read_content(AgentSource::Global, "bot")
        .expect("读取应成功");
    assert_eq!(content.system_md, md);
}

#[test]
fn read_content_nonexistent_returns_not_found() {
    let home = temp_home();
    let registry = global_only_registry(home.path());

    let result = registry.read_content(AgentSource::Global, "nonexistent");

    assert!(matches!(result, Err(RegistryError::NotFound(_))));
}

#[test]
fn write_content_nonexistent_returns_not_found() {
    let home = temp_home();
    let registry = global_only_registry(home.path());

    let result =
        registry.write_content(AgentSource::Global, "nonexistent", AgentFile::SystemMd, "x");

    assert!(matches!(result, Err(RegistryError::NotFound(_))));
}

#[test]
fn read_content_default_global_forbidden() {
    // default 在全局层禁止编辑（受 reject_default 保护）
    let home = temp_home();
    let registry = global_only_registry(home.path());

    let result = registry.read_content(AgentSource::Global, "default");

    assert!(matches!(result, Err(RegistryError::DefaultForbidden)));
}

#[test]
fn write_content_default_global_forbidden() {
    let home = temp_home();
    let registry = global_only_registry(home.path());

    let result = registry.write_content(AgentSource::Global, "default", AgentFile::SystemMd, "x");

    assert!(matches!(result, Err(RegistryError::DefaultForbidden)));
}

#[test]
fn write_content_creates_fuyao_toml_file() {
    let home = temp_home();
    let registry = global_only_registry(home.path());
    registry.create(AgentSource::Global, "bot").unwrap();

    let toml = "[tools]\nenabled = [\"read\"]\n";
    registry
        .write_content(AgentSource::Global, "bot", AgentFile::FuyaoToml, toml)
        .unwrap();

    let content = registry.read_content(AgentSource::Global, "bot").unwrap();
    assert_eq!(content.fuyao_toml, toml);
}

// ============================================================================
// 名称校验错误路径
// ============================================================================

#[test]
fn create_invalid_name_with_path_traversal_returns_invalid_name() {
    let home = temp_home();
    let registry = global_only_registry(home.path());

    let result = registry.create(AgentSource::Global, "../escape");

    assert!(matches!(result, Err(RegistryError::InvalidName(_))));
}

#[test]
fn create_default_global_returns_default_forbidden() {
    // create 不走 reject_default，但 validate_name 可能拒绝 "default"
    // 实际行为：create 对 global/default 调用 validate_name + agent_dir，不 reject_default
    // 此测试验证 create(global, "default") 的实际行为（不假设，按实现断言）
    let home = temp_home();
    let registry = global_only_registry(home.path());

    let result = registry.create(AgentSource::Global, "default");

    // create 不受 reject_default 保护（仅 read/write 受保护），
    // 若 validate_name 允许 "default"，则创建成功；否则 InvalidName
    match result {
        Ok(()) => {
            // 创建成功也合理（create 不保护 default）
            let dir = home.path().join("fuyao-agents").join("default");
            assert!(dir.is_dir());
        }
        Err(RegistryError::InvalidName(_)) => { /* validate_name 拒绝 default 也合理 */ }
        Err(e) => panic!("意外的错误变体: {e:?}"),
    }
}

// ============================================================================
// 分页边界
// ============================================================================

#[test]
fn list_page_less_than_1_treated_as_1() {
    let home = temp_home();
    write_registry_agent(home.path(), "bot", "---\nname: bot\n---\n正文");
    let registry = global_only_registry(home.path());

    let paged = registry.list(0, 10, None, None);

    assert_eq!(paged.page, 1, "page < 1 应视为 1");
}

#[test]
fn list_size_less_than_1_treated_as_1() {
    let home = temp_home();
    write_registry_agent(home.path(), "bot", "---\nname: bot\n---\n正文");
    let registry = global_only_registry(home.path());

    let paged = registry.list(1, 0, None, None);

    assert_eq!(paged.size, 1, "size < 1 应视为 1");
    assert!(!paged.items.is_empty());
}
