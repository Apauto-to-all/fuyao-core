//! 落地层：调 store.mark_compaction 写入压缩边界
//!
//! 流程：
//! 1. 调 [`SessionStore::mark_compaction`]（事务内 INSERT compaction 边界消息 +
//!    UPDATE sessions 元数据），返回新 seq
//!
//! 不复制 keep_recent：可见窗口的组装已迁移到读取侧 [`SessionStore::load_visible_messages`]
//! 的动态拼接（以最新摘要为起点向前切 keep_recent，用原始记录不产生副本）。本函数只负责
//! 把摘要落成 compaction 边界消息，DB 内消息保持唯一、无重复行。

use crate::SessionStore;
use crate::compressor::summary::SummaryResult;
use crate::error::SessionError;
use crate::store::compaction::CompressionReason;

/// 落地压缩结果：写 compaction 边界消息进 DB
///
/// 返回新 compaction 边界的 seq（供上层 CompressionEnded 事件携带，前端定位压缩位置）。
///
/// 不切窗口、不复制 keep_recent——可见窗口在读取侧 [`SessionStore::load_visible_messages`]
/// 动态拼接（摘要 + 以最新摘要为起点向前切的 keep_recent + 摘要后的新消息）。摘要生成所需的
/// 可见消息也由 [`SessionStore::load_visible_messages`] 提供。
pub async fn apply(
    summary: &SummaryResult,
    session_id: &str,
    store: &SessionStore,
) -> Result<i64, SessionError> {
    // 写 compaction 边界（事务内 INSERT + UPDATE sessions）
    let new_seq = store
        .mark_compaction(session_id, summary.content.clone(), CompressionReason::Auto)
        .await?;

    tracing::info!(
        session_id = session_id,
        new_seq = new_seq,
        "上下文压缩已应用（写入 compaction 边界）"
    );

    Ok(new_seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::{Message, MessageKind};
    use tempfile::tempdir;

    async fn temp_store() -> SessionStore {
        let dir = tempdir().expect("创建临时目录失败");
        let db_path = dir.path().join("test.db");
        std::mem::forget(dir);
        SessionStore::new(db_path).await.expect("创建存储失败")
    }

    #[tokio::test]
    async fn apply_inserts_boundary_without_cloning_keep_recent() {
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();

        // 10 条消息逐条插入（事件级落库模式）
        for i in 0..10 {
            let mut msg = Message::user(format!("消息_{i}_{}", "x".repeat(40)));
            store.insert_message(&session.id, &mut msg).await.unwrap();
        }

        let summary = SummaryResult {
            content: "## 目标\n- 测试".to_string(),
        };

        let new_seq = apply(&summary, &session.id, &store).await.unwrap();

        // compaction 边界 seq 应在原 10 条之后（11）
        assert_eq!(new_seq, 11);

        // 压缩后 DB 全量历史 = 原始 10 条 + 1 条 compaction 边界，无 keep_recent 副本
        let full = store.load_full_history(&session.id).await.unwrap();
        assert_eq!(
            full.len(),
            11,
            "全量 = 原始 10 + compaction 边界 1，不应有 keep_recent 副本"
        );

        // 唯一的 compaction 边界消息在 seq == new_seq
        let boundary = full
            .iter()
            .find(|m| m.kind == MessageKind::Compaction)
            .expect("应存在 compaction 边界消息");
        assert_eq!(boundary.seq, new_seq);
        assert_eq!(boundary.content.as_deref(), Some("## 目标\n- 测试"));
    }
}
