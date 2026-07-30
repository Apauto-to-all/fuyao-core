//! stdio MCP 端到端测试（需本地真实 MCP server 子进程）
//!
//! 验证 fuyao-mcp 的完整调用链：stdio 连接 → 工具发现 → call_tool 正常返回。
//! 排除 HTTP / 云端 / 限流干扰，专注检验本地子进程调用链是否健康。
//!
//! 标记 `#[ignore]`：依赖本机 `npx` + `@upstash/context7-mcp` 包，非纯单元测试。
//! 手动运行：`cargo test -p fuyao-mcp --test stdio_e2e_test -- --ignored --nocapture`

use std::collections::HashMap;

use fuyao_api::{CancellationToken, MCPServerConfig, ToolCallContext};
use fuyao_mcp::MCPManager;

/// 用本地 context7（stdio）验证 MCP 调用链端到端正常
///
/// 走引擎实际路径：get_tool_entries 拿到 bridge 生成的 handler，直接调用。
/// 这样完整覆盖 bridge.rs 的超时保护 + connection.rs 的 call_tool，与引擎行为一致。
#[tokio::test]
#[ignore]
async fn stdio_context7_call_tool_returns_result() {
    let mut configs = HashMap::new();
    configs.insert(
        "context7".to_string(),
        MCPServerConfig {
            command: Some("npx".to_string()),
            args: Some(vec!["-y".to_string(), "@upstash/context7-mcp".to_string()]),
            ..Default::default()
        },
    );

    let mgr = MCPManager::new(configs);

    // 1. 启动连接 + 发现工具
    let (success, failure, failures) = mgr.start_all().await;
    assert_eq!(success, 1, "context7 应启动成功，失败信息：{failures:?}");
    assert_eq!(failure, 0, "不应有失败");

    // 2. 取工具 entries（含 bridge 生成的 handler）——与引擎装配路径一致
    let entries = mgr.get_tool_entries().await;
    assert!(!entries.is_empty(), "应发现 context7 工具");
    println!("[e2e] 发现 {} 个工具", entries.len());

    // 找 resolve-library-id 工具（连字符被 sanitize 成下划线）
    let target = entries
        .iter()
        .find(|(name, _, _)| name.contains("resolve_library_id"))
        .expect("应找到 resolve-library-id 工具");

    // 3. 调用 handler（与引擎 execute_single 走完全相同的路径）
    let ctx = ToolCallContext::default();
    let cancel = CancellationToken::new();
    let args = serde_json::json!({
        "libraryName": "React",
        "query": "hooks"
    });

    let call_result = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        (target.2)(args, ctx, cancel),
    )
    .await;

    match call_result {
        Ok(result_str) => {
            println!("[e2e] ✅ 调用成功返回，结果长度 {} 字符", result_str.len());
            assert!(
                !result_str.contains("\"error\""),
                "不应返回 error，实际：{result_str}"
            );
        }
        Err(_) => panic!("[e2e] ❌ 调用超时（60 秒无响应），调用链卡死"),
    }

    // 4. 清理
    mgr.stop_all().await;
}
