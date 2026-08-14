//! 输入事件定义
//!
//! 定义 UI 层发送给 Engine 层的所有事件类型。
//! 每个事件对应一个枚举变体，envelope + payload 放在独立文件中管理。
//!
//! - `User`: 用户消息（envelope {base, payload}）
//! - `Interrupt`: 中断信号（envelope {base, payload}）
//! - `Compress`: 手动压缩请求（envelope {base}，无业务载荷）
//! - `Rollback`: 对话回退请求（envelope {base, payload}）

// 子模块：每种事件类型独立文件管理
mod compress_request;
mod interrupt;
mod rollback_request;
mod user;

// envelope / payload 在 input 层导出（外部通过 input::UserMessage 等路径访问）
pub use compress_request::CompressRequest;
pub use interrupt::{InterruptMessage, InterruptPayload, InterruptSource};
pub use rollback_request::{RollbackPayload, RollbackRequest};
pub use user::{
    PluginSource, SystemSource, UserMessage, UserMessageMode, UserMessageSource, UserPayload,
};

/// 输入事件（UI → Engine）
///
/// envelope 每变体自带 base（在 envelope struct 内），enum 保持穷尽匹配。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum InputEvent {
    /// 用户消息：用户在界面中发送的消息
    User(UserMessage),
    /// 中断：用户取消或中断当前操作
    Interrupt(InterruptMessage),
    /// 手动压缩请求：用户 / 上层应用主动请求一次上下文压缩
    ///
    /// 经引擎入口送入控制通道，在 turn 边界触发压缩（跳过阈值与反抖动，
    /// 复用自动压缩执行流程，触发原因标记为 manual）。
    Compress(CompressRequest),
    /// 对话回退请求：用户 / 上层应用请求把会话回退到某条消息
    ///
    /// 经引擎入口送入控制通道，在 turn 边界触发回退执行（删目标 seq 之后的所有消息 +
    /// 重算会话状态）。结果经 per-session 出口以 [`OutputEvent::Rollback`] 事件流出，
    /// 不走请求-响应通道——与压缩的 Started/Ended 三阶段事件同机制。
    Rollback(RollbackRequest),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;

    #[test]
    fn user_event_contains_content_and_mode() {
        let event = InputEvent::User(UserMessage {
            base: EventBase::default(),
            payload: UserPayload {
                content: "测试消息".into(),
                images: vec![],
                mode: UserMessageMode::Guide,
                source: UserMessageSource::User,
            },
        });
        match &event {
            InputEvent::User(msg) => {
                assert_eq!(msg.payload.content, "测试消息");
                assert_eq!(msg.payload.mode, UserMessageMode::Guide);
                assert_eq!(msg.payload.source, UserMessageSource::User);
            }
            _ => panic!("应为 User 变体"),
        }
        let _ = event; // 确保 Clone 可用
    }

    #[test]
    fn interrupt_event_contains_reason() {
        let event = InputEvent::Interrupt(InterruptMessage {
            base: EventBase::default(),
            payload: InterruptPayload {
                reason: "用户取消".into(),
                source: InterruptSource::User,
            },
        });
        match &event {
            InputEvent::Interrupt(msg) => {
                assert_eq!(msg.payload.reason, "用户取消");
                assert_eq!(msg.payload.source, InterruptSource::User);
            }
            _ => panic!("应为 Interrupt 变体"),
        }
    }

    #[test]
    fn compress_event_carries_base() {
        let event = InputEvent::Compress(CompressRequest {
            base: EventBase::default(),
        });
        match &event {
            InputEvent::Compress(req) => assert!(req.base.seq.is_none()),
            _ => panic!("应为 Compress 变体"),
        }
    }

    #[test]
    fn input_event_serde_roundtrip() {
        let event = InputEvent::User(UserMessage {
            base: EventBase::default(),
            payload: UserPayload {
                content: "序列化".into(),
                images: vec![],
                mode: UserMessageMode::Guide,
                source: UserMessageSource::User,
            },
        });
        let json = serde_json::to_string(&event).expect("序列化失败");
        let de: InputEvent = serde_json::from_str(&json).expect("反序列化失败");
        match de {
            InputEvent::User(msg) => assert_eq!(msg.payload.content, "序列化"),
            _ => panic!("反序列化后应为 User 变体"),
        }
    }

    #[test]
    fn compress_event_serde_roundtrip() {
        let event = InputEvent::Compress(CompressRequest {
            base: EventBase::default(),
        });
        let json = serde_json::to_string(&event).expect("序列化失败");
        let de: InputEvent = serde_json::from_str(&json).expect("反序列化失败");
        match de {
            InputEvent::Compress(req) => assert!(req.base.seq.is_none()),
            _ => panic!("反序列化后应为 Compress 变体"),
        }
    }

    #[test]
    fn rollback_event_carries_target_seq() {
        let event = InputEvent::Rollback(RollbackRequest {
            base: EventBase::default(),
            payload: RollbackPayload { target_seq: 9 },
        });
        match &event {
            InputEvent::Rollback(req) => {
                assert!(req.base.seq.is_none());
                assert_eq!(req.payload.target_seq, 9);
            }
            _ => panic!("应为 Rollback 变体"),
        }
        let _ = event; // 确保 Clone 可用
    }

    #[test]
    fn rollback_event_serde_roundtrip() {
        let event = InputEvent::Rollback(RollbackRequest {
            base: EventBase::default(),
            payload: RollbackPayload { target_seq: 12 },
        });
        let json = serde_json::to_string(&event).expect("序列化失败");
        let de: InputEvent = serde_json::from_str(&json).expect("反序列化失败");
        match de {
            InputEvent::Rollback(req) => assert_eq!(req.payload.target_seq, 12),
            _ => panic!("反序列化后应为 Rollback 变体"),
        }
    }
}
