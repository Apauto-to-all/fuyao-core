//! 落地层：调 store.mark_compaction + 复制 keep_recent 为新 seq
//!
//! 流程：
//! 1. 调 [`SessionStore::mark_compaction`]（事务内 INSERT compaction 边界消息 +
//!    UPDATE sessions 元数据），返回新 seq
//! 2. 切 keep_recent 窗口（与 summary 层切的一致）
//! 3. **把 keep_recent 复制成新 seq 重新 INSERT**——让下次 `load_visible_messages`
//!    能自然看到（compaction 边界 seq + 新复制的 keep_recent seq 都 >= 边界 seq）。
//!
//! 与原「保留原 seq 写回内存」方案的差异（设计变更）：
//! - 消息列表已从内存移到 DB，原方案下 keep_recent 的 seq < compaction 边界 seq，
//!   会被 `load_visible_messages` 的 `seq >= last_compacted_seq` 过滤掉——丢失近期上下文。
//! - 复制成新 seq 后，DB 里有两份 keep_recent（旧 seq 留作审计、新 seq 作可见窗口），
//!   冗余存储可接受（keep_recent 受 keep_tokens_max 上限，几 KB 量级）。

use crate::SessionStore;
use crate::compressor::summary::SummaryResult;
use crate::compressor::window::select_recent;
use crate::error::SessionError;
use crate::store::compaction::CompressionReason;
use fuyao_api::{CompressionConfig, Message};

/// 落地压缩结果：写 compaction 边界 + 复制 keep_recent 为新 seq 进 DB
///
/// 返回新 compaction 边界的 seq（供上层 CompressionEnded 事件携带，前端定位压缩位置）。
///
/// `context_length` 用于按比例计算保留窗口预算，必须与 `generate_summary` 传入的值一致，
/// 保证 summary 层与 apply 层切的是同一个窗口。
///
/// `messages` 是调用方从 DB 查询的当前可见消息（apply 内部不再写回内存——内存已无 messages 字段）。
pub async fn apply(
    messages: &[Message],
    summary: &SummaryResult,
    session_id: &str,
    cfg: &CompressionConfig,
    context_length: u32,
    store: &SessionStore,
) -> Result<i64, SessionError> {
    // 1. 写 compaction 边界（事务内 INSERT + UPDATE sessions）
    let new_seq = store
        .mark_compaction(session_id, summary.content.clone(), CompressionReason::Auto)
        .await?;

    // 2. 重新切窗口（与 summary 层切的一致——基于原 messages）
    // 保留预算按模型上下文比例动态计算
    let keep_tokens = cfg.effective_keep_tokens(context_length);
    let window = select_recent(messages, keep_tokens);

    // 3. 把 keep_recent 复制成新 seq 插入（让下次 load_visible_messages 能看到）
    //    clone 后强制 seq=0，insert_message 会分配新 seq（> new_seq）
    for msg in window.keep_recent {
        let mut clone = msg.clone();
        clone.seq = 0;
        store.insert_message(session_id, &mut clone).await?;
    }

    tracing::info!(
        session_id = session_id,
        new_seq = new_seq,
        before_count = messages.len(),
        kept_count = window.keep_recent.len(),
        tokens_before = summary.tokens_before,
        tokens_after = summary.tokens_after,
        "上下文压缩已应用（compaction 边界 + 复制 keep_recent 为新 seq）"
    );

    Ok(new_seq)
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
    async fn apply_inserts_boundary_and_clones_keep_recent() {
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None);
        store.create(&session).await.unwrap();

        // 10 条消息逐条插入（事件级落库模式）
        for i in 0..10 {
            let mut msg = Message::user(format!("消息_{i}_{}", "x".repeat(40)));
            store.insert_message(&session.id, &mut msg).await.unwrap();
        }

        // 压缩前的可见消息（全部 10 条）
        let before = store.load_visible_messages(&session.id).await.unwrap();
        assert_eq!(before.len(), 10);

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
        let new_seq = apply(&before, &summary, &session.id, &cfg, 128_000, &store)
            .await
            .unwrap();

        // compaction 边界 seq 应在原 10 条之后（11）
        assert!(new_seq > 0);

        // 压缩后可见窗口 = [compaction 边界] + [keep_recent 副本]
        let after = store.load_visible_messages(&session.id).await.unwrap();
        assert!(!after.is_empty());

        // 第一条是 compaction 边界，seq == new_seq
        assert_eq!(after[0].kind, MessageKind::Compaction);
        assert_eq!(after[0].content.as_deref(), Some("## 目标\n- 测试"));
        assert_eq!(after[0].seq, new_seq);

        // 之后的是 keep_recent 副本（kind = Message）
        for m in &after[1..] {
            assert_eq!(m.kind, MessageKind::Message);
            assert!(
                m.seq > new_seq,
                "keep_recent 副本 seq 应大于 compaction 边界 seq"
            );
        }

        // 可见窗口显著短于原始（compaction 边界 + 少量 keep_recent 副本）
        assert!(after.len() < before.len());

        // 全量历史仍保留所有原消息（审计）
        let full = store.load_full_history(&session.id).await.unwrap();
        assert_eq!(
            full.len(),
            10 + 1 + (after.len() - 1),
            "全量 = 原始 10 + compaction 边界 + keep_recent 副本"
        );
    }
}
