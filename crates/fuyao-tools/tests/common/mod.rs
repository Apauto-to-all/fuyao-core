//! fuyao-tools 集成测试共享 fixture
//!
//! 构造 ToolCallContext（注入 workspace + 唯一 session_id），
//! 并提供调用 handler 的便捷函数。session_id 唯一以规避全局 TRACKER 去重串扰。

// 跨测试二进制共享：未用部分不报 dead_code
#![allow(dead_code)]

use fuyao_api::{AgentPaths, CancellationToken, ToolCallContext, ToolFn};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);

/// 构造带 workspace 注入 + 唯一 session_id 的 ToolCallContext
///
/// 唯一 session_id 避免全局 TRACKER 跨测试去重命中。
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

/// 调用工具 handler，返回解析后的 JSON
pub async fn call_tool(handler: &ToolFn, args: Value, ctx: &ToolCallContext) -> Value {
    let result = handler(args, ctx.clone(), CancellationToken::new()).await;
    serde_json::from_str(&result).unwrap_or_else(|_| {
        Value::Object(serde_json::Map::from_iter([(
            "raw".to_string(),
            Value::String(result),
        )]))
    })
}
