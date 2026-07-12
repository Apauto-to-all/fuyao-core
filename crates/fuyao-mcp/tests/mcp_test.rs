//! fuyao-mcp 集成测试（方案 C：静态/构造行为 + 错误路径）
//!
//! MCPManager 是「重运行时 I/O + 私有实现」类型（依赖真实子进程 + 私有 connection 模块），
//! 无法简单 mock。本测试覆盖不启动真实进程即可验证的行为：
//! - 构造（new 空/带配置、from_config 走 default 兜底）
//! - 状态机初始态（空 manager 的各查询方法）
//! - 错误路径（call_tool/refresh_tools 对不存在目标的错误变体）
//! - stop_all 幂等、stop_server 永远 Ok 契约
//! - start_all 全 disabled 跳过、start_server 失败传播
//!
//! 明确不覆盖（需真实子进程，超出方案 C 范围）：
//! call_tool/refresh_tools 成功路径、bridge 熔断/恢复、structuredContent 处理。

use std::collections::HashMap;

use fuyao_api::MCPServerConfig;
use fuyao_mcp::{MCPManager, MCPManagerError};

// ---------------------------------------------------------------------------
// 构造：new
// ---------------------------------------------------------------------------

#[tokio::test]
async fn new_with_empty_configs_has_no_servers() {
    let mgr = MCPManager::new(HashMap::new());
    assert!(mgr.get_configured_servers().is_empty());
    assert!(mgr.get_server_status().await.is_empty());
    assert!(mgr.get_registered_tools().await.is_empty());
}

#[tokio::test]
async fn new_lists_configured_server_names() {
    let mut configs = HashMap::new();
    configs.insert(
        "stdio-srv".to_string(),
        MCPServerConfig {
            command: Some("echo".to_string()),
            ..Default::default()
        },
    );
    configs.insert(
        "http-srv".to_string(),
        MCPServerConfig {
            url: Some("http://localhost:1234".to_string()),
            ..Default::default()
        },
    );

    let mgr = MCPManager::new(configs);
    let mut names = mgr.get_configured_servers();
    names.sort();
    assert_eq!(names, vec!["http-srv", "stdio-srv"]);
}

// ---------------------------------------------------------------------------
// 构造：from_config 走 default 兜底
// ---------------------------------------------------------------------------

#[tokio::test]
async fn from_config_returns_empty_when_not_set() {
    // 未调用 set_config（集成测试不触碰 OnceLock），get_config 返回 default
    // default 的 mcp_servers 为空 → from_config 构造出空 manager
    let mgr = MCPManager::from_config();
    assert!(mgr.get_configured_servers().is_empty());
}

// ---------------------------------------------------------------------------
// 状态机初始态：空 manager 的查询方法
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_manager_all_queries_return_empty() {
    let mgr = MCPManager::new(HashMap::new());
    assert!(mgr.get_registered_tools().await.is_empty());
    assert!(mgr.get_tool_definitions().await.is_empty());
    assert!(mgr.get_tool_entries().await.is_empty());
    assert!(mgr.get_server_status().await.is_empty());
}

// ---------------------------------------------------------------------------
// call_tool 错误路径
// ---------------------------------------------------------------------------

#[tokio::test]
async fn call_tool_unknown_returns_tool_not_found_with_empty_server() {
    // 未注册任何工具 → ToolNotFound，且 server 字段为空串（钉死现有契约）
    let mgr = MCPManager::new(HashMap::new());
    let result = mgr
        .call_tool("nonexistent_tool", serde_json::json!({}))
        .await;
    match result {
        Err(MCPManagerError::ToolNotFound { server, tool }) => {
            assert_eq!(tool, "nonexistent_tool");
            assert!(server.is_empty(), "未注册工具时 server 字段应为空串");
        }
        other => panic!("应为 ToolNotFound，实际：{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// refresh_tools 错误路径
// ---------------------------------------------------------------------------

#[tokio::test]
async fn refresh_tools_unknown_server_returns_server_not_found() {
    let mgr = MCPManager::new(HashMap::new());
    let result = mgr.refresh_tools("nonexistent_server").await;
    assert!(
        matches!(result, Err(MCPManagerError::ServerNotFound(ref name)) if name == "nonexistent_server"),
        "不存在的 server 应返回 ServerNotFound"
    );
}

// ---------------------------------------------------------------------------
// stop_server / stop_all 契约
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stop_server_unknown_returns_ok_anyway() {
    // stop_server 对不存在的 server 也返回 Ok（钉死现有契约：移除操作幂等）
    let mgr = MCPManager::new(HashMap::new());
    let result = mgr.stop_server("no-such-server").await;
    assert!(result.is_ok(), "stop_server 不存在 server 应返回 Ok");
}

#[tokio::test]
async fn stop_all_on_empty_manager_is_idempotent() {
    let mgr = MCPManager::new(HashMap::new());
    // 连续两次 stop_all 都不应 panic
    mgr.stop_all().await;
    mgr.stop_all().await;
    // stop_all 后 registered_tools 仍为空
    assert!(mgr.get_registered_tools().await.is_empty());
}

// ---------------------------------------------------------------------------
// start_all：全 disabled 跳过、空 configs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn start_all_empty_configs_returns_zero_success() {
    let mgr = MCPManager::new(HashMap::new());
    let (success, failure, failures) = mgr.start_all().await;
    assert_eq!(success, 0);
    assert_eq!(failure, 0);
    assert!(failures.is_empty());
}

#[tokio::test]
async fn start_all_all_disabled_skips_without_starting() {
    // 所有 server enabled=false → 全部跳过，不启动任何子进程
    let mut configs = HashMap::new();
    configs.insert(
        "disabled-srv".to_string(),
        MCPServerConfig {
            command: Some("echo".to_string()),
            enabled: false,
            ..Default::default()
        },
    );

    let mgr = MCPManager::new(configs);
    let (success, failure, failures) = mgr.start_all().await;
    assert_eq!(success, 0, "disabled server 不应计入成功");
    assert_eq!(failure, 0, "disabled server 跳过，不计入失败");
    assert!(failures.is_empty());
}

// ---------------------------------------------------------------------------
// start_server 失败传播（方案 C 触达最深处，不依赖真实 server）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn start_server_with_nonexistent_command_returns_start_failed() {
    // 给一个绝对不存在的 command，子进程 spawn 会失败
    // 这验证了 MCPConnection::start 的错误传播到 MCPManagerError::ServerStartFailed
    let mgr = MCPManager::new(HashMap::new());
    let config = MCPServerConfig {
        command: Some("nonexistent_cmd_xyz_99999".to_string()),
        ..Default::default()
    };

    let result = mgr.start_server("bad-srv", config).await;
    match result {
        Err(MCPManagerError::ServerStartFailed { server, reason }) => {
            assert_eq!(server, "bad-srv");
            assert!(!reason.is_empty(), "失败原因应非空");
        }
        other => panic!("应为 ServerStartFailed，实际：{other:?}"),
    }

    // 失败后不应留下连接或注册工具
    assert!(mgr.get_registered_tools().await.is_empty());
    assert!(mgr.get_server_status().await.is_empty());
}

#[tokio::test]
async fn start_all_with_bad_command_records_failure_but_continues() {
    // 混合配置：一个坏 command + 一个 disabled，验证坏的不阻塞、disabled 跳过
    let mut configs = HashMap::new();
    configs.insert(
        "bad".to_string(),
        MCPServerConfig {
            command: Some("nonexistent_cmd_xyz_99999".to_string()),
            ..Default::default()
        },
    );
    configs.insert(
        "off".to_string(),
        MCPServerConfig {
            command: Some("echo".to_string()),
            enabled: false,
            ..Default::default()
        },
    );

    let mgr = MCPManager::new(configs);
    let (success, failure, failures) = mgr.start_all().await;
    assert_eq!(success, 0, "坏 command 不应成功");
    assert_eq!(failure, 1, "坏 command 应计入失败");
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].0, "bad", "失败记录应含坏 server 名");
}

// ---------------------------------------------------------------------------
// get_server_status：未连接时不含条目
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_server_status_empty_before_any_start() {
    let mut configs = HashMap::new();
    configs.insert(
        "srv".to_string(),
        MCPServerConfig {
            command: Some("echo".to_string()),
            ..Default::default()
        },
    );
    let mgr = MCPManager::new(configs);

    // 配置了 server 但未 start → status 为空（status 跟踪连接而非配置）
    let status = mgr.get_server_status().await;
    assert!(status.is_empty(), "未 start 时 status 应为空");
}
