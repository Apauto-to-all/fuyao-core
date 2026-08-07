//! 事件基类
//!
//! 所有事件数据结构的公共字段，由每个数据 struct 嵌入。
//! 后续新增公共字段只需改此处，所有事件自动获得。

use std::time::{SystemTime, UNIX_EPOCH};

/// 事件基类
///
/// 嵌入到每个事件数据 struct 中作为 `base` 字段。
/// 包含消息序号、时间戳、会话标识，后续可扩展 source、trace_id 等字段。
///
/// `session_id` 是多 session 并发的全程标签：入口消息和出口事件的结构都带它，
/// 从入口到出口一路跟随，消费者据此分流。
///
/// `seq` 是会话内单调递增的消息序号，由 store 层在落库时分配（见
/// [`crate::Message::seq`]）。只有进历史的事件（User/Assistant/ToolResult）有 seq；
/// 不落库的纯实时事件（Chunk/Error/Compression/Interrupt 通知等）为 `None`。实时事件
/// 的 seq 与历史回放读回的 seq 同构——前端游标分页据此连续定位，无需区分实时/历史。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EventBase {
    /// 会话内消息序号（落库时由 store 层分配）
    ///
    /// 仅进历史的事件为 `Some(seq)`；不落库的纯实时事件为 `None`。
    /// `None` 序列化时不输出该字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
    /// 事件时间戳（Unix 纪元秒，含小数）
    pub timestamp: f64,
    /// 会话标识（多 session 全程标签）
    ///
    /// `None` 表示事件未归属到特定 session。
    /// 缺该字段的旧 JSON 反序列化为 `None`；`None` 序列化时不输出该字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl Default for EventBase {
    fn default() -> Self {
        Self {
            seq: None,
            timestamp: current_timestamp(),
            session_id: None,
        }
    }
}

impl EventBase {
    /// 更新时间戳为当前时刻
    pub fn update_timestamp(&mut self) {
        self.timestamp = current_timestamp();
    }
}

/// 获取当前 Unix 时间戳（秒，含小数）
fn current_timestamp() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_base_default_seq_is_none() {
        // 默认构造：seq 为 None（纯实时事件无 seq，落库后由调用方回填）
        let base = EventBase::default();
        assert!(base.seq.is_none());
    }

    #[test]
    fn event_base_default_has_valid_timestamp() {
        let base = EventBase::default();
        assert!(base.timestamp > 0.0);
    }

    #[test]
    fn event_base_clone_works() {
        let base = EventBase {
            seq: Some(42),
            ..Default::default()
        };
        let cloned = base.clone();
        assert_eq!(base.seq, cloned.seq);
        assert_eq!(base.timestamp, cloned.timestamp);
    }

    #[test]
    fn event_base_debug_works() {
        let base = EventBase::default();
        let debug_str = format!("{:?}", base);
        assert!(debug_str.contains("seq"));
        assert!(debug_str.contains("timestamp"));
    }

    #[test]
    fn update_timestamp_changes_value() {
        let mut base = EventBase::default();
        let original_ts = base.timestamp;
        std::thread::sleep(std::time::Duration::from_millis(10));
        base.update_timestamp();
        assert!(base.timestamp > original_ts);
    }

    #[test]
    fn session_id_defaults_to_none() {
        let base = EventBase::default();
        assert!(base.session_id.is_none());
    }

    /// 验证：session_id = None 时序列化不输出该字段（保持输出干净）
    #[test]
    fn session_id_none_skipped_in_serialization() {
        let base = EventBase::default();
        let json = serde_json::to_string(&base).expect("序列化失败");
        assert!(
            !json.contains("session_id"),
            "None 的 session_id 不应出现在序列化输出中: {json}"
        );
    }

    /// 验证：seq = None 时序列化不输出该字段（保持输出干净）
    #[test]
    fn seq_none_skipped_in_serialization() {
        let base = EventBase::default();
        let json = serde_json::to_string(&base).expect("序列化失败");
        assert!(
            !json.contains("\"seq\""),
            "None 的 seq 不应出现在序列化输出中: {json}"
        );
    }

    /// 验证：seq = Some 时正常序列化 / 反序列化（round-trip）
    #[test]
    fn seq_some_round_trip() {
        let base = EventBase {
            seq: Some(7),
            ..Default::default()
        };
        let json = serde_json::to_string(&base).expect("序列化失败");
        let decoded: EventBase = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(decoded.seq, Some(7));
    }

    /// 验证：session_id = Some 时正常序列化 / 反序列化（round-trip）
    #[test]
    fn session_id_some_round_trip() {
        let base = EventBase {
            session_id: Some("sess-123".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_string(&base).expect("序列化失败");
        let decoded: EventBase = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(decoded.session_id.as_deref(), Some("sess-123"));
    }

    /// 验证：缺 seq 字段的旧 JSON 反序列化为 None（向后兼容）
    #[test]
    fn seq_missing_field_decodes_as_none() {
        let old_json = r#"{"timestamp":1.5}"#;
        let decoded: EventBase = serde_json::from_str(old_json).expect("反序列化失败");
        assert!(decoded.seq.is_none());
        assert!((decoded.timestamp - 1.5).abs() < f64::EPSILON);
    }
}
