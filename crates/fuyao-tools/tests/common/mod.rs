//! fuyao-tools 集成测试共享 fixture
//!
//! 构造 ToolCallContext（注入 workspace + 唯一 session_id），
//! 并提供调用 handler 的便捷函数。session_id 唯一以规避全局 TRACKER 去重串扰。

// 跨测试二进制共享：未用部分不报 dead_code
#![allow(dead_code)]

use fuyao_api::{AgentPaths, CancellationToken, ToolCallContext, ToolFn};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);

/// 构造带 workspace 注入 + 唯一 session_id 的 ToolCallContext
///
/// 唯一 session_id 避免全局 TRACKER 跨测试去重命中。
/// 不注入存储能力——给不需要 store 的工具（read/write/glob 等）用。
pub fn make_ctx(workspace: PathBuf) -> ToolCallContext {
    let n = SESSION_SEQ.fetch_add(1, Ordering::SeqCst);
    ToolCallContext {
        session_id: Some(format!("it_tools_{n}")),
        agent_paths: Some(AgentPaths {
            agent_id: None,
            workspace: Some(workspace),
            extra_dirs: Vec::new(),
            fuyao_home: std::env::temp_dir().join("fuyao_it_tools_home"),
        }),
        ..ToolCallContext::default()
    }
}

/// 构造带 workspace + 真实 SessionStore 注入的 ToolCallContext
///
/// 在 make_ctx 基础上额外注入会话存储（实现 [`fuyao_api::TodoStoreOps`]），
/// 供需要存储能力的工具（todo）做端到端集成测试。store 指向独立临时目录的
/// 数据库，与其它测试隔离。
pub async fn make_ctx_with_store(workspace: PathBuf) -> ToolCallContext {
    // 独立临时目录承载 sessions.db，避免跨测试数据串扰。
    // forget 让目录留到进程结束（SessionStore 句柄跨 await 存活，dir 必须存活）。
    let dir = tempfile::tempdir().expect("创建临时目录失败");
    let db_path = dir.path().join("test.db");
    std::mem::forget(dir);

    let store = Arc::new(
        fuyao_session::SessionStore::new(db_path)
            .await
            .expect("创建 SessionStore 失败"),
    );

    let n = SESSION_SEQ.fetch_add(1, Ordering::SeqCst);
    ToolCallContext {
        session_id: Some(format!("it_tools_{n}")),
        agent_paths: Some(AgentPaths {
            agent_id: None,
            workspace: Some(workspace),
            extra_dirs: Vec::new(),
            fuyao_home: std::env::temp_dir().join("fuyao_it_tools_home"),
        }),
        capabilities: fuyao_api::ToolCapabilities {
            todo_store: Some(store),
            ..Default::default()
        },
        ..ToolCallContext::default()
    }
}

/// 调用工具 handler，返回解析后的 JSON
///
/// Value/Err 变体的 wire 均为 JSON 可直接解析；纯文本结果（Text 变体）
/// 包成 `{"raw": 原文}` 供断言使用。
pub async fn call_tool(handler: &ToolFn, args: Value, ctx: &ToolCallContext) -> Value {
    let output = handler(args, ctx.clone(), CancellationToken::new()).await;
    let wire = output.to_wire();
    serde_json::from_str(&wire).unwrap_or_else(|_| {
        Value::Object(serde_json::Map::from_iter([(
            "raw".to_string(),
            Value::String(wire),
        )]))
    })
}
