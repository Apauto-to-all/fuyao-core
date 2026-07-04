//! 插件输入事件
//!
//! 插件通过 SendInputFn 钩子发送插件通知（警告、状态等）。
//! 引擎收到后转发为 OutputEvent::Plugin 通知 UI。
//!
//! 字段与 event_output::PluginData 保持一致，
//! 转换通过 event_output::PluginData 的 From 实现。

use crate::message::EventBase;

/// 插件输入数据
///
/// 插件通过 SendInputFn 发送，引擎主循环接收后
/// 通过 emit(OutputEvent::Plugin(data.into())) 通知 UI。
#[derive(Debug, Clone)]
pub struct PluginData {
    /// 事件基类（时间戳等公共字段）
    pub base: EventBase,
    /// 来源插件名称
    pub source: String,
    /// 事件类型
    pub event_type: String,
    /// 事件数据
    pub data: Option<serde_json::Value>,
    /// 错误信息
    pub error: Option<String>,
    /// 提醒信息
    pub message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_data_holds_fields() {
        let data = PluginData {
            base: EventBase::default(),
            source: "loop_guard".to_string(),
            event_type: "loop_warn".to_string(),
            data: None,
            error: None,
            message: Some("检测到循环".to_string()),
        };
        assert_eq!(data.source, "loop_guard");
        assert_eq!(data.event_type, "loop_warn");
        assert_eq!(data.message, Some("检测到循环".to_string()));
    }
}
