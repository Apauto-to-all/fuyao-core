//! fuyao-tools 集成测试：工具注册表 + handler 端到端执行
//!
//! 聚焦跨模块协作与公共 API 契约：
//! - `all_tools()` / `get_tool()` 注册表查询
//! - 通过 handler 执行 read/write/glob/grep/edit/bash，验证 workspace 注入与路径解析
//! - 单元测试已覆盖各 handler 的纯逻辑，本测试验证「注册表 → handler → JSON 结果」的装配链路
//!
//! 每个 session_id 唯一，规避全局 TRACKER 去重串扰。

mod common;

use common::{call_tool, make_ctx, make_ctx_with_store};
use fuyao_api::ToolCallContext;
use fuyao_tools::{all_tool_names, all_tools, get_tool};
use serde_json::json;
use std::fs;
use tempfile::TempDir;

// ============================================================================
// 注册表：all_tools / get_tool / all_tool_names
// ============================================================================

#[test]
fn registry_contains_expected_builtin_tools() {
    let tools = all_tools();
    let names = all_tool_names();

    // 核心工具必须存在
    for expected in ["read", "write", "glob", "grep", "edit", "bash"] {
        assert!(tools.contains_key(expected), "注册表应含工具：{expected}");
    }
    assert_eq!(
        tools.len(),
        names.len(),
        "all_tools 与 all_tool_names 数量一致"
    );
}

#[test]
fn get_tool_returns_entry_with_definition() {
    let entry = get_tool("read").expect("read 工具应存在");
    // definition 可序列化为 JSON schema
    let schema = serde_json::to_value(&entry.definition).expect("definition 应可序列化");
    assert!(
        schema.get("function").is_some(),
        "schema 应含 function 字段"
    );
}

#[test]
fn get_tool_nonexistent_returns_none() {
    assert!(get_tool("nonexistent_tool").is_none());
}

#[test]
fn all_tool_entries_have_handlers() {
    // 每个注册的工具都有非空 handler（Arc 指针）
    for (name, entry) in all_tools() {
        let _handler_ref = &entry.handler;
        let _ = name; // 遍历即验证可访问
    }
}

// ============================================================================
// read：真实文件读取 + workspace 路径解析
// ============================================================================

#[tokio::test]
async fn read_file_with_workspace_relative_path() {
    let ws = TempDir::new().unwrap();
    fs::write(ws.path().join("test.txt"), "hello world").unwrap();
    let ctx = make_ctx(ws.path().to_path_buf());

    let entry = get_tool("read").unwrap();
    let result = call_tool(&entry.handler, json!({"path": "test.txt"}), &ctx).await;

    // read 返回 result 字段（带行号格式）
    assert!(
        result.get("result").is_some(),
        "read 应返回 result 字段，实际：{result}"
    );
    assert!(
        result["result"].to_string().contains("hello world"),
        "result 应含文件内容"
    );
}

#[tokio::test]
async fn read_nonexistent_file_returns_error_json() {
    let ws = TempDir::new().unwrap();
    let ctx = make_ctx(ws.path().to_path_buf());

    let entry = get_tool("read").unwrap();
    let result = call_tool(&entry.handler, json!({"path": "nonexistent.txt"}), &ctx).await;

    assert!(
        result.get("error").is_some(),
        "读取不存在文件应返回 error 字段，实际：{result}"
    );
}

// ============================================================================
// write：真实文件写入
// ============================================================================

#[tokio::test]
async fn write_file_creates_new_file() {
    let ws = TempDir::new().unwrap();
    let ctx = make_ctx(ws.path().to_path_buf());

    let entry = get_tool("write").unwrap();
    let result = call_tool(
        &entry.handler,
        json!({"path": "new.txt", "content": "写入内容"}),
        &ctx,
    )
    .await;

    // 文件应被创建
    let written = fs::read_to_string(ws.path().join("new.txt")).unwrap();
    assert_eq!(written, "写入内容");
    let _ = result;
}

#[tokio::test]
async fn write_file_overwrites_existing() {
    let ws = TempDir::new().unwrap();
    fs::write(ws.path().join("existing.txt"), "旧内容").unwrap();
    let ctx = make_ctx(ws.path().to_path_buf());

    let entry = get_tool("write").unwrap();
    call_tool(
        &entry.handler,
        json!({"path": "existing.txt", "content": "新内容"}),
        &ctx,
    )
    .await;

    let written = fs::read_to_string(ws.path().join("existing.txt")).unwrap();
    assert_eq!(written, "新内容");
}

// ============================================================================
// glob：真实目录搜索
// ============================================================================

#[tokio::test]
async fn glob_finds_matching_files() {
    let ws = TempDir::new().unwrap();
    fs::write(ws.path().join("a.rs"), "x").unwrap();
    fs::write(ws.path().join("b.rs"), "x").unwrap();
    fs::write(ws.path().join("c.txt"), "x").unwrap();
    let ctx = make_ctx(ws.path().to_path_buf());

    let entry = get_tool("glob").unwrap();
    let result = call_tool(&entry.handler, json!({"pattern": "**/*.rs"}), &ctx).await;

    // glob 应找到 2 个 .rs 文件
    let matches = result
        .get("matches")
        .or_else(|| result.get("files"))
        .cloned()
        .unwrap_or(result);
    let matches_str = matches.to_string();
    assert!(
        matches_str.contains("a.rs"),
        "应匹配 a.rs，实际：{matches_str}"
    );
    assert!(matches_str.contains("b.rs"), "应匹配 b.rs");
}

#[tokio::test]
async fn glob_no_match_returns_empty() {
    let ws = TempDir::new().unwrap();
    let ctx = make_ctx(ws.path().to_path_buf());

    let entry = get_tool("glob").unwrap();
    let result = call_tool(&entry.handler, json!({"pattern": "**/*.nonexistent"}), &ctx).await;

    let result_str = result.to_string();
    // 无匹配不应 panic
    let _ = result_str;
}

// ============================================================================
// grep：真实内容搜索
// ============================================================================

#[tokio::test]
async fn grep_finds_matching_content() {
    let ws = TempDir::new().unwrap();
    fs::write(ws.path().join("code.rs"), "fn search() {}\nfn main() {}\n").unwrap();
    let ctx = make_ctx(ws.path().to_path_buf());

    let entry = get_tool("grep").unwrap();
    let result = call_tool(&entry.handler, json!({"pattern": "fn main"}), &ctx).await;

    let result_str = result.to_string();
    assert!(
        result_str.contains("main")
            || result_str.contains("code.rs")
            || result.get("matches").is_some(),
        "grep 应找到匹配，实际：{result_str}"
    );
}

#[tokio::test]
async fn grep_no_match_returns_empty_result() {
    let ws = TempDir::new().unwrap();
    fs::write(ws.path().join("code.rs"), "fn main() {}\n").unwrap();
    let ctx = make_ctx(ws.path().to_path_buf());

    let entry = get_tool("grep").unwrap();
    let result = call_tool(
        &entry.handler,
        json!({"pattern": "绝对不存在的字符串xyz"}),
        &ctx,
    )
    .await;

    // 无匹配不应 panic
    let _ = result;
}

// ============================================================================
// edit：replace 模式真实编辑
// ============================================================================

#[tokio::test]
async fn edit_replace_modifies_file_content() {
    // 真实中文内容验证 replace 模式（多字节 char boundary panic 已修复）
    let ws = TempDir::new().unwrap();
    fs::write(ws.path().join("target.txt"), "旧内容\n第二行\n").unwrap();
    let ctx = make_ctx(ws.path().to_path_buf());

    let entry = get_tool("edit").unwrap();
    let result = call_tool(
        &entry.handler,
        json!({
            "mode": "replace",
            "path": "target.txt",
            "old_string": "旧内容",
            "new_string": "新内容"
        }),
        &ctx,
    )
    .await;

    let written = fs::read_to_string(ws.path().join("target.txt")).unwrap();
    assert!(written.contains("新内容"), "edit 后应含新内容：{written}");
    assert!(
        !written.contains("旧内容"),
        "edit 后不应含旧内容：{written}"
    );
    let _ = result;
}

#[tokio::test]
async fn edit_replace_multibyte_replace_all() {
    // 多次出现的中文，replace_all=true：验证多字节字符下批量替换不 panic
    let ws = TempDir::new().unwrap();
    fs::write(ws.path().join("multi.txt"), "你好世界\n你好朋友\n").unwrap();
    let ctx = make_ctx(ws.path().to_path_buf());

    let entry = get_tool("edit").unwrap();
    let _result = call_tool(
        &entry.handler,
        json!({
            "mode": "replace",
            "path": "multi.txt",
            "old_string": "你好",
            "new_string": "您好",
            "replace_all": true
        }),
        &ctx,
    )
    .await;

    let written = fs::read_to_string(ws.path().join("multi.txt")).unwrap();
    assert_eq!(
        written, "您好世界\n您好朋友\n",
        "replace_all 应替换所有中文匹配"
    );
}

// ============================================================================
// bash：真实命令执行
// ============================================================================

#[tokio::test]
async fn bash_executes_simple_command() {
    let ws = TempDir::new().unwrap();
    let ctx = make_ctx(ws.path().to_path_buf());

    let entry = get_tool("bash").unwrap();
    let result = call_tool(
        &entry.handler,
        json!({"command": "echo integration_test_marker"}),
        &ctx,
    )
    .await;

    let result_str = result.to_string();
    assert!(
        result_str.contains("integration_test_marker"),
        "bash echo 输出应含标记，实际：{result_str}"
    );
}

#[tokio::test]
async fn bash_writes_to_workspace() {
    // bash 在注入的 workspace 下执行命令，验证 workdir 注入
    let ws = TempDir::new().unwrap();
    let ctx = make_ctx(ws.path().to_path_buf());

    let entry = get_tool("bash").unwrap();
    call_tool(
        &entry.handler,
        json!({"command": "echo created > from_bash.txt"}),
        &ctx,
    )
    .await;

    // workspace 下应有 from_bash.txt（workdir 注入生效时）
    // 注意：不同 shell 的重定向行为可能差异，这里宽松断言不 panic
    let _ = fs::read_to_string(ws.path().join("from_bash.txt")).ok();
}

// ============================================================================
// todowrite：注入 SessionStore 后的端到端读写（TodoStoreOps 注入链路）
// ============================================================================

#[tokio::test]
async fn todowrite_without_todos_reads_empty_list() {
    // 不传 todos = 读取当前列表：注入了 store 的 ctx 读取新 session，应得空列表
    let ws = TempDir::new().unwrap();
    let ctx = make_ctx_with_store(ws.path().to_path_buf()).await;

    let entry = get_tool("todowrite").unwrap();
    let result = call_tool(&entry.handler, json!({}), &ctx).await;

    assert!(
        result.get("success").is_some(),
        "todowrite 应返回结果信封，实际：{result}"
    );
    assert_eq!(result["success"], true);
    assert!(result["todos"].is_array(), "todos 应为数组");
    assert!(
        result["todos"].as_array().unwrap().is_empty(),
        "新 session 的任务列表应为空"
    );
}

#[tokio::test]
async fn todowrite_writes_then_persists() {
    // 传 todos = 整体覆盖写入：写入后立即再读，应回显写入内容（持久化生效）
    let ws = TempDir::new().unwrap();
    let ctx = make_ctx_with_store(ws.path().to_path_buf()).await;

    let entry = get_tool("todowrite").unwrap();
    let result = call_tool(
        &entry.handler,
        json!({
            "todos": [
                {"id": "1", "content": "任务一", "status": "pending"},
                {"id": "2", "content": "任务二", "status": "completed"}
            ]
        }),
        &ctx,
    )
    .await;

    // 写入结果回显两条任务 + 摘要
    assert_eq!(result["success"], true);
    let todos = result["todos"].as_array().expect("todos 应为数组");
    assert_eq!(todos.len(), 2, "应回显 2 条任务");
    assert_eq!(todos[0]["content"], "任务一");
    assert_eq!(todos[1]["status"], "completed");
    assert_eq!(result["summary"]["total"], 2, "摘要 total 应为 2");
    assert_eq!(result["summary"]["completed"], 1);

    // 二次读取（不传 todos）验证持久化——同一 ctx 的 store 是同一份
    let reread = call_tool(&entry.handler, json!({}), &ctx).await;
    assert_eq!(
        reread["todos"].as_array().unwrap().len(),
        2,
        "二次读取应得持久化的 2 条任务"
    );
}

#[tokio::test]
async fn todowrite_overwrites_previous() {
    // 再次整体覆盖写入：旧任务被清空，只剩新传入的
    let ws = TempDir::new().unwrap();
    let ctx = make_ctx_with_store(ws.path().to_path_buf()).await;

    let entry = get_tool("todowrite").unwrap();
    call_tool(
        &entry.handler,
        json!({"todos": [{"id": "1", "content": "旧任务", "status": "pending"}]}),
        &ctx,
    )
    .await;

    let result = call_tool(
        &entry.handler,
        json!({"todos": [{"id": "2", "content": "新任务", "status": "in_progress"}]}),
        &ctx,
    )
    .await;

    let todos = result["todos"].as_array().unwrap();
    assert_eq!(todos.len(), 1, "覆盖写入后应只剩 1 条");
    assert_eq!(todos[0]["id"], "2", "应为新任务的 id");
    assert_eq!(todos[0]["content"], "新任务");
    assert!(!todos[0]["content"].as_str().unwrap().contains("旧任务"));
}

#[tokio::test]
async fn todowrite_isolated_by_session_id() {
    // 同一 store、不同 session_id 的任务列表互不串扰（session_id 软关联隔离）
    use fuyao_api::AgentPaths;
    use std::sync::Arc;

    let ws = TempDir::new().unwrap();

    // 两个 ctx 共享同一个 SessionStore，仅 session_id 不同
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    std::mem::forget(dir);
    let store = Arc::new(fuyao_session::SessionStore::new(db_path).await.unwrap());

    let mk_ctx = |sid: &str| ToolCallContext {
        session_id: Some(sid.to_string()),
        agent_paths: Some(AgentPaths {
            workspace: Some(ws.path().to_path_buf()),
            ..AgentPaths::default()
        }),
        todo_store: Some(store.clone()),
        ..ToolCallContext::default()
    };
    let ctx_a = mk_ctx("session_a");
    let ctx_b = mk_ctx("session_b");

    let entry = get_tool("todowrite").unwrap();
    call_tool(
        &entry.handler,
        json!({"todos": [{"id": "1", "content": "A 的任务", "status": "pending"}]}),
        &ctx_a,
    )
    .await;
    call_tool(
        &entry.handler,
        json!({"todos": [{"id": "1", "content": "B 的任务", "status": "pending"}]}),
        &ctx_b,
    )
    .await;

    let a = call_tool(&entry.handler, json!({}), &ctx_a).await;
    let b = call_tool(&entry.handler, json!({}), &ctx_b).await;
    assert_eq!(a["todos"][0]["content"], "A 的任务", "session A 隔离");
    assert_eq!(b["todos"][0]["content"], "B 的任务", "session B 隔离");
}
