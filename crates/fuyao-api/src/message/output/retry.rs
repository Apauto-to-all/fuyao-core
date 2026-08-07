//! LLM 重试事件
//!
//! 引擎在 LLM 调用失败后判定可重试时，进入退避等待并发出本事件，前端据此渲染
//! 「重试中…N 秒后重试，错误：xxx」提示。事件经统一消息处理管道（dispatch）发送：
//! 可被 output_intercept 钩子拦截改写，也会触发 output_observe 钩子（如审计日志）。
//!
//! 事件触发点：`fuyao-core/src/react/retry.rs::RetryRunner`（per-session，每次重试前发一条）。
//!
//! 与 Title 一样采用 envelope + payload 两层结构（扁平 payload，无阶段区分），
//! 对齐项目既有模式。区别于 Compression 的 Started/Delta/Ended 三阶段——重试是离散的
//! 一次性通知，每次重试独立发一条，不需要阶段标识。

use crate::message::EventBase;

/// LLM 重试事件 envelope
///
/// 携带 `base`（事件元信息 + session_id 全程标签）和 `payload`（重试详情）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RetryMessage {
    /// 事件元信息（seq/timestamp/session_id）
    pub base: EventBase,
    /// 重试载荷
    pub payload: RetryPayload,
}

/// 重试载荷
///
/// 每个字段都对应 UI 渲染所需的 1 条信息：
/// - `attempt`：第几次重试（1-based，1 = 第一次重试）
/// - `max_retries`：配置的最大重试次数（UI 可显示「2/5」；u32::MAX 时 UI 应渲染为「无限」）
/// - `wait_ms`：本次退避等待毫秒数（UI 用它显示「N 秒后重试」倒计时）
/// - `cause`：错误的人类可读描述（UI 显示错误原因）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RetryPayload {
    /// 第几次重试（1-based，1 = 第一次重试）
    pub attempt: u32,
    /// 配置的最大重试次数（u32::MAX = 无限重试）
    pub max_retries: u32,
    /// 本次退避等待毫秒数
    pub wait_ms: u64,
    /// 错误的人类可读描述
    pub cause: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_carries_retry_info() {
        let payload = RetryPayload {
            attempt: 2,
            max_retries: 5,
            wait_ms: 4000,
            cause: "速率限制".into(),
        };
        assert_eq!(payload.attempt, 2);
        assert_eq!(payload.max_retries, 5);
        assert_eq!(payload.wait_ms, 4000);
        assert_eq!(payload.cause, "速率限制");
    }

    #[test]
    fn message_envelope_constructs() {
        let msg = RetryMessage {
            base: EventBase::default(),
            payload: RetryPayload {
                attempt: 1,
                max_retries: 3,
                wait_ms: 2000,
                cause: "连接超时".into(),
            },
        };
        assert!(msg.base.timestamp > 0.0);
        assert_eq!(msg.payload.attempt, 1);
    }

    #[test]
    fn message_clone_works() {
        let msg = RetryMessage {
            base: EventBase::default(),
            payload: RetryPayload {
                attempt: 1,
                max_retries: 3,
                wait_ms: 2000,
                cause: "原始".into(),
            },
        };
        let cloned = msg.clone();
        assert_eq!(cloned.payload.attempt, msg.payload.attempt);
        assert_eq!(cloned.base.seq, msg.base.seq);
    }

    #[test]
    fn payload_serde_roundtrip() {
        let original = RetryMessage {
            base: EventBase::default(),
            payload: RetryPayload {
                attempt: 3,
                max_retries: 5,
                wait_ms: 8000,
                cause: "服务不可用".into(),
            },
        };
        let json = serde_json::to_string(&original).expect("序列化失败");
        let restored: RetryMessage = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(restored.payload.attempt, 3);
        assert_eq!(restored.payload.cause, "服务不可用");
    }

    #[test]
    fn message_with_session_id_roundtrip() {
        let mut msg = RetryMessage {
            base: EventBase::default(),
            payload: RetryPayload {
                attempt: 1,
                max_retries: 3,
                wait_ms: 2000,
                cause: "测试".into(),
            },
        };
        msg.base.session_id = Some("sess-abc".into());

        let json = serde_json::to_string(&msg).expect("序列化失败");
        assert!(
            json.contains(r#""session_id":"sess-abc""#),
            "session_id 应出现在 JSON 中: {json}"
        );

        let restored: RetryMessage = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(restored.base.session_id.as_deref(), Some("sess-abc"));
    }
}
