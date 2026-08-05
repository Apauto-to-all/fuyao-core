//! 对话回退事件
//!
//! 回退完成后发出本事件，前端据此：
//! - 显示「已回退 N 条消息」通知（`deleted_count` 口径 = 撤掉的用户消息 + 压缩消息数）
//! - 把目标用户消息内容填入输入框供重新编辑 / 发送（`target_message`）
//! - 定位当前会话位置（`target_seq`，回退后它是最新消息的 seq）
//!
//! 事件经统一消息处理管道（dispatch）发送：可被 output_intercept 钩子拦截改写，
//! 也会触发 output_observe 钩子（如审计日志）。
//!
//! # payload 的双重消费者
//!
//! `RollbackPayload` 既是输出事件的载荷，也是 store 层 `rollback_to` 的返回类型——
//! 一处定义，store 产出（单事务内重算好全部字段）+ engine 包装成 `RollbackMessage`
//! 发出，两端复用。回退发起方（如活跃 session 回退的 `handle_control` 分支）还可
//! 拿 payload 里的状态刷新字段就地刷新内存 session 对象，避免后续 persist 全量写回时
//! 把旧值盖回去。

use crate::message::EventBase;
use crate::message::input::{UserMessageMode, UserMessageSource};
use crate::message::output::UserPayload;

/// 对话回退事件 envelope
///
/// 携带 `base`（事件元信息 + session_id 全程标签）和 `payload`（回退结果）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RollbackMessage {
    /// 事件元信息（id/timestamp/session_id）
    pub base: EventBase,
    /// 回退载荷
    pub payload: RollbackPayload,
}

/// 对话回退载荷
///
/// 字段按消费者分两组：
///
/// - **锚点 / 界面反馈**：`target_seq` / `deleted_count` / `deleted_total` / `target_message`
/// - **状态刷新**（供回退发起方就地刷新内存 session 对象）：`message_count` /
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
    /// 不污染回退事件。
    ///
    /// - 目标是 user 消息 → `Some`，前端把 content + images 填入输入框供重新编辑 / 发送
    /// - 目标是 compaction 消息 → `None`，压缩摘要是助手产出，不填输入框
    #[serde(default)]
    pub target_message: Option<UserPayload>,
    /// 重算后的消息总数（刷内存 session 用）
    pub message_count: i64,
    /// 重算后的工具调用总数（刷内存 session 用）
    pub tool_call_count: i64,
    /// 重算后的最新压缩边界 seq（刷内存 session 用）
    ///
    /// 回退跨压缩边界时会变：若删掉了所有 compaction 消息，置 `None`（从未压缩）；
    /// 否则落到剩余消息里最新一条 compaction 消息的 seq。
    pub last_compacted_seq: Option<i64>,
    /// 重算后的压缩次数（刷内存 session 用）
    pub compression_count: i32,
}

impl RollbackPayload {
    /// 从目标 user 消息构造填输入框用的 `UserPayload`
    ///
    /// 供 store 层构造返回值用——回退后用户重新编辑发送时，它就是一条新的 guide 消息，
    /// 故 mode 取 `Guide`、source 取 `User`（原消息的 mode/source 语义已不适用）。
    pub fn user_payload_from(
        content: Option<String>,
        images: Vec<crate::ImageContent>,
    ) -> UserPayload {
        UserPayload {
            content: content.unwrap_or_default(),
            images,
            mode: UserMessageMode::Guide,
            source: UserMessageSource::User,
        }
    }
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
    fn message_envelope_constructs() {
        let msg = RollbackMessage {
            base: EventBase::default(),
            payload: RollbackPayload {
                target_seq: 3,
                deleted_count: 1,
                deleted_total: 1,
                target_message: None,
                message_count: 3,
                tool_call_count: 0,
                last_compacted_seq: None,
                compression_count: 0,
            },
        };
        assert!(msg.base.timestamp > 0.0);
        assert_eq!(msg.payload.target_seq, 3);
    }

    #[test]
    fn message_clone_works() {
        let msg = RollbackMessage {
            base: EventBase::default(),
            payload: RollbackPayload {
                target_seq: 1,
                deleted_count: 0,
                deleted_total: 0,
                target_message: None,
                message_count: 1,
                tool_call_count: 0,
                last_compacted_seq: None,
                compression_count: 0,
            },
        };
        let cloned = msg.clone();
        assert_eq!(cloned.payload.target_seq, msg.payload.target_seq);
        assert_eq!(cloned.base.id, msg.base.id);
    }

    #[test]
    fn payload_serde_roundtrip() {
        let original = RollbackMessage {
            base: EventBase::default(),
            payload: RollbackPayload {
                target_seq: 7,
                deleted_count: 2,
                deleted_total: 4,
                target_message: None,
                message_count: 6,
                tool_call_count: 1,
                last_compacted_seq: Some(2),
                compression_count: 1,
            },
        };
        let json = serde_json::to_string(&original).expect("序列化失败");
        let restored: RollbackMessage = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(restored.payload.target_seq, 7);
        assert_eq!(restored.payload.deleted_count, 2);
        assert_eq!(restored.payload.last_compacted_seq, Some(2));
    }

    #[test]
    fn message_with_session_id_roundtrip() {
        // 验证 session_id 标签随事件流过管道后仍可恢复
        let mut msg = RollbackMessage {
            base: EventBase::default(),
            payload: RollbackPayload {
                target_seq: 1,
                deleted_count: 0,
                deleted_total: 0,
                target_message: None,
                message_count: 1,
                tool_call_count: 0,
                last_compacted_seq: None,
                compression_count: 0,
            },
        };
        msg.base.session_id = Some("sess-abc".into());

        let json = serde_json::to_string(&msg).expect("序列化失败");
        assert!(
            json.contains(r#""session_id":"sess-abc""#),
            "session_id 应出现在 JSON 中: {json}"
        );

        let restored: RollbackMessage = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(restored.base.session_id.as_deref(), Some("sess-abc"));
    }
}
