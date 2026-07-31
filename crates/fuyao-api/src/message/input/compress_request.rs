//! 压缩请求输入事件
//!
//! 定义用户 / 上层应用主动请求上下文压缩的输入消息。该消息经引擎入口送入控制通道，
//! 在 turn 边界触发一次手动压缩（跳过阈值与反抖动，复用自动压缩的执行流程，
//! 触发原因标记为 manual）。

use crate::message::EventBase;

/// 压缩请求消息 envelope
///
/// 由 [`crate::message::input::InputEvent::Compress`] 携带。无业务载荷——压缩请求只有
/// 「现在就压」一个意图，触发参数（模型、上下文长度等）由引擎在执行时按 session 配置现解析。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompressRequest {
    /// 事件元信息（id/timestamp）
    pub base: EventBase,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compress_request_carries_base() {
        let req = CompressRequest {
            base: EventBase::default(),
        };
        assert!(!req.base.id.is_empty());
    }

    #[test]
    fn compress_request_serde_roundtrip() {
        let req = CompressRequest {
            base: EventBase::default(),
        };
        let json = serde_json::to_string(&req).expect("序列化失败");
        let de: CompressRequest = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(de.base.id, req.base.id);
    }
}
