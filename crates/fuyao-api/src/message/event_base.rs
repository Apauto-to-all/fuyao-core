//! 事件基类
//!
//! 所有事件数据结构的公共字段，由每个数据 struct 嵌入。
//! 后续新增公共字段只需改此处，所有事件自动获得。

use std::time::{SystemTime, UNIX_EPOCH};

/// 事件基类
///
/// 嵌入到每个事件数据 struct 中作为 `base` 字段。
/// 包含唯一 ID、时间戳、会话标识，后续可扩展 source、trace_id 等字段。
///
/// `session_id` 是多 session 并发的全程标签：入口消息和出口事件的结构都带它，
/// 从入口到出口一路跟随，消费者据此分流。
/// 引擎级事件（如全局 Shutdown）可为 `None`；会话相关事件必须为 `Some`。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EventBase {
    /// 事件唯一 ID（UUID v4）
    pub id: String,
    /// 事件时间戳（Unix 纪元秒，含小数）
    pub timestamp: f64,
    /// 会话标识（多 session 全程标签）
    ///
    /// `None` 表示事件未归属到特定 session（引擎级事件，如全局 Shutdown）。
    /// 缺该字段的旧 JSON 反序列化为 `None`；`None` 序列化时不输出该字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl Default for EventBase {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
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
    fn event_base_default_has_valid_id() {
        let base = EventBase::default();
        assert!(!base.id.is_empty());
        assert!(base.id.len() == 36); // UUID v4 format: xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
    }

    #[test]
    fn event_base_default_has_valid_timestamp() {
        let base = EventBase::default();
        assert!(base.timestamp > 0.0);
    }

    #[test]
    fn event_base_clone_works() {
        let base = EventBase::default();
        let cloned = base.clone();
        assert_eq!(base.id, cloned.id);
        assert_eq!(base.timestamp, cloned.timestamp);
    }

    #[test]
    fn event_base_debug_works() {
        let base = EventBase::default();
        let debug_str = format!("{:?}", base);
        assert!(debug_str.contains("id"));
        assert!(debug_str.contains("timestamp"));
    }

    #[test]
    fn event_base_ids_are_unique() {
        let base1 = EventBase::default();
        let base2 = EventBase::default();
        assert_ne!(base1.id, base2.id);
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

    /// 验证：缺 session_id 字段的旧 JSON 反序列化为 None（向后兼容）
    #[test]
    fn session_id_missing_field_decodes_as_none() {
        let old_json = r#"{"id":"abc","timestamp":1.5}"#;
        let decoded: EventBase = serde_json::from_str(old_json).expect("反序列化失败");
        assert_eq!(decoded.id, "abc");
        assert!((decoded.timestamp - 1.5).abs() < f64::EPSILON);
        assert!(decoded.session_id.is_none());
    }
}
