//! 数据库行映射：sessions / messages 表 ↔ 领域类型（Session / Message）

use fuyao_api::{Message, Session};
use sqlx::FromRow;

/// 会话行（数据库列映射，字段顺序与 sessions 表一致）
#[derive(FromRow)]
pub(super) struct SessionRow {
    pub(super) id: String,
    pub(super) parent_session_id: Option<String>,
    pub(super) started_at: f64,
    pub(super) ended_at: Option<f64>,
    pub(super) end_reason: Option<String>,
    pub(super) message_count: i64,
    pub(super) tool_call_count: i64,
    pub(super) total_prompt_tokens: i64,
    pub(super) total_completion_tokens: i64,
    pub(super) total_reasoning_tokens: i64,
    pub(super) total_cached_tokens: i64,
    pub(super) total_cost: f64,
    pub(super) title: Option<String>,
    pub(super) system_prompt: Option<String>,
}

impl From<SessionRow> for Session {
    fn from(r: SessionRow) -> Self {
        Session {
            id: r.id,
            parent_session_id: r.parent_session_id,
            title: r.title,
            system_prompt: r.system_prompt,
            message_count: r.message_count,
            tool_call_count: r.tool_call_count,
            total_prompt_tokens: r.total_prompt_tokens,
            total_completion_tokens: r.total_completion_tokens,
            total_reasoning_tokens: r.total_reasoning_tokens,
            total_cached_tokens: r.total_cached_tokens,
            total_cost: r.total_cost,
            started_at: r.started_at,
            ended_at: r.ended_at,
            end_reason: r.end_reason,
            messages: Vec::new(),
        }
    }
}

/// 消息行（数据库列映射，字段顺序与 messages 表一致）
#[derive(FromRow)]
pub(super) struct MessageRow {
    pub(super) id: Option<i64>,
    pub(super) session_id: String,
    pub(super) model_id: Option<String>,
    pub(super) role: String,
    pub(super) content: Option<String>,
    pub(super) tool_call_id: Option<String>,
    pub(super) tool_calls: Option<String>,
    pub(super) tool_name: Option<String>,
    pub(super) timestamp: f64,
    pub(super) prompt_tokens: i64,
    pub(super) completion_tokens: i64,
    pub(super) reasoning_tokens: i64,
    pub(super) cached_tokens: i64,
    pub(super) cost: f64,
    pub(super) finish_reason: Option<String>,
    pub(super) reasoning: Option<String>,
}

impl From<MessageRow> for Message {
    fn from(r: MessageRow) -> Self {
        let tool_calls = r.tool_calls.and_then(|s| match serde_json::from_str(&s) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(cause = %e, "tool_calls 反序列化失败，已丢弃");
                None
            }
        });
        Message {
            id: r.id,
            session_id: r.session_id,
            model_id: r.model_id,
            role: r.role,
            content: r.content,
            reasoning: r.reasoning,
            tool_call_id: r.tool_call_id,
            tool_calls,
            tool_name: r.tool_name,
            finish_reason: r.finish_reason,
            timestamp: r.timestamp,
            prompt_tokens: r.prompt_tokens,
            completion_tokens: r.completion_tokens,
            reasoning_tokens: r.reasoning_tokens,
            cached_tokens: r.cached_tokens,
            cost: r.cost,
        }
    }
}
