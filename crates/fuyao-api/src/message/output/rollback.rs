//! 对话回退结果载荷
//!
//! 会话管理门面（`SessionManager::rollback_session`）的返回值——回退执行的
//! wire 投影，请求-响应语义直接返调用方（不经事件流）。应用层据此：
//! - 显示「已回退 N 条消息」通知（`deleted_count` 口径 = 撤掉的用户消息 + 压缩消息数）
//! - 把目标用户消息内容填入输入框供重新编辑 / 发送（`target_message`）
//! - 定位当前会话位置（`target_seq`，回退后它是最新消息的 seq）
//!
//! # 载荷来源
//!
//! 持久化层（`fuyao_session::RollbackResult`）只陈述回退的领域事实；本 payload 是
//! 它的 wire 投影——由管理门面按「目标消息将作为新 guide 重新发送」补上
//! `mode`/`source`（应用语义不入 store 层）。

use crate::message::output::UserPayload;

/// 对话回退载荷
///
/// 字段按消费者分两组：
///
/// - **锚点 / 界面反馈**：`target_seq` / `deleted_count` / `deleted_total` / `target_message`
/// - **状态刷新**（供调用方对齐会话元数据）：`message_count` /
///   `tool_call_count` / `last_compacted_seq` / `compression_count`
///
/// 消费类字段（token / cost）不在载荷中——回退只改消息列表，不否定历史真实消费，
/// 这些字段在 DB 里原值保留。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RollbackPayload {
    /// 回退到的目标 seq（位置锚点）
    ///
    /// 回退后它是当前最新消息的 seq。上层据此定位「现在在哪」，后续一切操作
    /// （可见窗口、继续对话）都以它为起点。
    pub target_seq: i64,
    /// 删除的「用户消息数 + 压缩消息数」
    ///
    /// 界面通知用——「已回退 N 条消息」对用户有意义的口径是「撤了几轮对话 + 撤了几次压缩」，
    /// 不含附属的 assistant / tool 响应。
    pub deleted_count: i64,
    /// 删除的全部消息数（含 assistant / tool）
    ///
    /// 审计 / 前端备用。回退会删目标 seq 之后的所有消息（含中间态），总数与
    /// `deleted_count` 的差就是被一并清理的 assistant / tool 消息数。
    pub deleted_total: i64,
    /// 目标用户消息（填输入框用，完整消息形态）
    ///
    /// 复用 [`UserPayload`]——项目里「用户消息输出形态」的既定类型，前端可直接套用
    /// 渲染用户消息的逻辑。未来扩展（音频 / 文件等）跟着 `UserPayload` 一起长，
    /// 不污染回退载荷。
    ///
    /// - 目标是 user 消息 → `Some`，前端把 content + images 填入输入框供重新编辑 / 发送
    /// - 目标是 compaction 消息 → `None`，压缩摘要是助手产出，不填输入框
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_message: Option<UserPayload>,
    /// 重算后的消息总数（刷会话元数据用）
    pub message_count: i64,
    /// 重算后的工具调用总数（刷会话元数据用）
    pub tool_call_count: i64,
    /// 重算后的最新压缩边界 seq（刷会话元数据用）
    ///
    /// 回退跨压缩边界时会变：若删掉了所有 compaction 消息，置 `None`（从未压缩）；
    /// 否则落到剩余消息里最新一条 compaction 消息的 seq。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compacted_seq: Option<i64>,
    /// 重算后的压缩次数（刷会话元数据用）
    pub compression_count: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_carries_rollback_fields() {
        let payload = RollbackPayload {
            target_seq: 5,
            deleted_count: 2,
            deleted_total: 3,
            target_message: None,
            message_count: 4,
            tool_call_count: 1,
            last_compacted_seq: Some(3),
            compression_count: 1,
        };
        assert_eq!(payload.target_seq, 5);
        assert_eq!(payload.deleted_count, 2);
        assert_eq!(payload.deleted_total, 3);
        assert!(payload.target_message.is_none());
    }

    #[test]
    fn payload_serde_roundtrip() {
        let original = RollbackPayload {
            target_seq: 7,
            deleted_count: 2,
            deleted_total: 4,
            target_message: None,
            message_count: 6,
            tool_call_count: 1,
            last_compacted_seq: Some(2),
            compression_count: 1,
        };
        let json = serde_json::to_string(&original).expect("序列化失败");
        let restored: RollbackPayload = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(restored.target_seq, 7);
        assert_eq!(restored.deleted_count, 2);
        assert_eq!(restored.last_compacted_seq, Some(2));
    }

    #[test]
    fn payload_target_message_optional_field_skips_when_none() {
        // target_message / last_compacted_seq 为 None 时不参与序列化（载荷精简）
        let payload = RollbackPayload {
            target_seq: 1,
            deleted_count: 0,
            deleted_total: 0,
            target_message: None,
            message_count: 1,
            tool_call_count: 0,
            last_compacted_seq: None,
            compression_count: 0,
        };
        let json = serde_json::to_string(&payload).expect("序列化失败");
        assert!(
            !json.contains("target_message"),
            "None 字段不应出现在 JSON 中: {json}"
        );
        assert!(
            !json.contains("last_compacted_seq"),
            "None 字段不应出现在 JSON 中: {json}"
        );
    }
}
