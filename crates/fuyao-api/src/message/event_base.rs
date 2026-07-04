//! 事件基类
//!
//! 所有事件数据结构的公共字段，由每个数据 struct 嵌入。
//! 后续新增公共字段只需改此处，所有事件自动获得。

use std::time::{SystemTime, UNIX_EPOCH};

/// 事件基类
///
/// 嵌入到每个事件数据 struct 中作为 `base` 字段。
/// 包含唯一 ID 和时间戳，后续可扩展 source、trace_id 等字段。
#[derive(Debug, Clone, serde::Serialize)]
pub struct EventBase {
    /// 事件唯一 ID（UUID v4）
    pub id: String,
    /// 事件时间戳（Unix 纪元秒，含小数）
    pub timestamp: f64,
}

impl Default for EventBase {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: current_timestamp(),
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
}
