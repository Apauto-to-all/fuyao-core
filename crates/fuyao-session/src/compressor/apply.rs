//! 落地层：调 store.mark_compaction + 内存重建
//!
//! 流程：
//! 1. 调 [`SessionStore::mark_compaction`]（事务内 INSERT compaction 边界消息 +
//!    UPDATE sessions 元数据），返回新 seq
//! 2. 构造内存新窗口 = [compaction 消息(seq=new_seq)] + [原 keep_recent 消息（保留原 seq）]
//! 3. 返回新窗口给主循环，主循环 `session.messages = result` 直接替换

use crate::SessionStore;
use crate::compressor::summary::SummaryResult;
use crate::compressor::window::select_recent;
use crate::error::SessionError;
use crate::store::compaction::CompressionReason;
use fuyao_api::{CompressionConfig, Message};

/// 落地压缩结果：写 compaction 边界 + 重建内存可见窗口
///
/// 返回 `(新 messages, 新 compaction 边界的 seq)`：
/// - 新 messages 数组**显著短于**输入：[compaction 边界] + [tail 保留窗口]。
///   主循环拿到后直接 `session.messages = result`。
/// - new_seq 用于上层发布 CompressionEnded 事件时携带（前端定位压缩在对话流中的位置）。
///
/// `context_length` 用于按比例计算保留窗口预算，必须与 `generate_summary` 传入的值一致，
/// 保证 summary 层与 apply 层切的是同一个窗口。
pub async fn apply(
    messages: &[Message],
    summary: &SummaryResult,
    session_id: &str,
    cfg: &CompressionConfig,
    context_length: u32,
    store: &SessionStore,
) -> Result<(Vec<Message>, i64), SessionError> {
    // 1. 写 compaction 边界（事务内 INSERT + UPDATE sessions）
    let new_seq = store
        .mark_compaction(session_id, summary.content.clone(), CompressionReason::Auto)
        .await?;

    // 2. 重新切窗口（与 summary 层切的一致——基于原 messages）
    // 保留预算按模型上下文比例动态计算，与 generate_summary 用同一公式
    let keep_tokens = cfg.effective_keep_tokens(context_length);
    let window = select_recent(messages, keep_tokens);

    // 3. 构造内存新窗口：[compaction 边界] + [keep_recent（保留原 seq）]
    let mut new_messages = Vec::with_capacity(1 + window.keep_recent.len());
    let mut boundary = Message::compaction(summary.content.clone());
    boundary.seq = new_seq;
    new_messages.push(boundary);
    new_messages.extend(window.keep_recent.iter().cloned());

    tracing::info!(
        session_id = session_id,
        new_seq = new_seq,
        before_count = messages.len(),
        after_count = new_messages.len(),
        tokens_before = summary.tokens_before,
        tokens_after = summary.tokens_after,
        "上下文压缩已应用"
    );

    Ok((new_messages, new_seq))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::MessageKind;
    use tempfile::tempdir;

    async fn temp_store() -> SessionStore {
        let dir = tempdir().expect("创建临时目录失败");
        let db_path = dir.path().join("test.db");
        std::mem::forget(dir);
        SessionStore::new(db_path).await.expect("创建存储失败")
    }

    #[tokio::test]
    async fn apply_rebuilds_messages_with_boundary_at_front() {
        let store = temp_store().await;
        let mut session = fuyao_api::Session::new(None, None);
        // 10 条消息，每条 ~16 token（用极小 keep_tokens 强制压缩）
        for i in 0..10 {
            session
                .messages
                .push(Message::user(format!("消息_{i}_{}", "x".repeat(40))));
        }
        store.create(&mut session).await.unwrap();

        let summary = SummaryResult {
            content: "## 目标\n- 测试".to_string(),
            tokens_before: 100,
            tokens_after: 50,
        };

        let cfg = CompressionConfig {
            keep_ratio: 1.0,     // 比例拉满，让 effective_keep_tokens 永远等于 keep_tokens_max
            keep_tokens_max: 50, // 极小预算，强制压缩多数消息
            ..CompressionConfig::default()
        };
        let (new_messages, new_seq) = apply(
            &session.messages,
            &summary,
            &session.id,
            &cfg,
            128_000,
            &store,
        )
        .await
        .unwrap();

        // apply 返回的 new_seq 与边界消息的 seq 一致
        assert!(new_seq > 0);

        // 第一条是 compaction 边界
        assert_eq!(new_messages[0].kind, MessageKind::Compaction);
        assert_eq!(new_messages[0].content.as_deref(), Some("## 目标\n- 测试"));
        assert_eq!(new_messages[0].seq, new_seq);

        // 总长度显著小于原始
        assert!(new_messages.len() < session.messages.len());

        // 后续消息是 tail 保留窗口（原 seq 保留）
        for m in &new_messages[1..] {
            assert_eq!(m.kind, MessageKind::Message);
        }
    }
}
