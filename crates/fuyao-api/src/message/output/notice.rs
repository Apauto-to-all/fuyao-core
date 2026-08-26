//! 插件通知输出事件
//!
//! 插件主动向消费者（CLI/TUI）发送的通知——如循环检测提示。
//! 纯实时事件：不落库（不进聊天历史，seq 恒 None）、不经插件拦截/观察面
//! （通知不可被其他插件 Block，也不进观察以防「观察→再通知」反馈环）。
//!
//! `source` 复用 input 侧 [`PluginSource`]（纯数据类型，无方向语义，
//! 先例：`InterruptSource` 输入输出共享）。

use crate::message::EventBase;
use crate::message::input::PluginSource;

/// 插件通知输出事件 envelope
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginNoticeMessage {
    /// 事件元信息（id/timestamp/session_id）
    pub base: EventBase,
    /// 通知载荷
    pub payload: PluginNoticePayload,
}

/// 通知级别（三级）
///
/// 供消费者决定渲染形态（一般提示 / 警告 / 错误样式）。
/// 循环检测插件的升级阶梯映射：Warn 档发 Warn，Interrupt/Abort 发 Error。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NoticeLevel {
    /// 一般提示
    Info,
    /// 警告
    Warn,
    /// 错误 / 严重告警
    Error,
}

/// 插件通知载荷
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginNoticePayload {
    /// 来源插件（名称可追溯）
    pub source: PluginSource,
    /// 通知级别
    pub level: NoticeLevel,
    /// 通知正文
    pub content: String,
}

impl PluginNoticePayload {
    /// 构造通知载荷
    ///
    /// 内核链路只认 output 侧类型，入口（SessionSender 直产）统一用此方法构造，
    /// 避免散落的字段照搬样板。
    pub fn new(source: impl Into<String>, level: NoticeLevel, content: impl Into<String>) -> Self {
        Self {
            source: PluginSource {
                name: source.into(),
            },
            level,
            content: content.into(),
        }
    }
}

impl PluginNoticeMessage {
    /// 构造通知事件（base 取默认值，自动生成 id/timestamp；session_id 由发送侧盖标签）
    pub fn new(source: impl Into<String>, level: NoticeLevel, content: impl Into<String>) -> Self {
        Self {
            base: EventBase::default(),
            payload: PluginNoticePayload::new(source, level, content),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 载荷持有全部字段：来源 / 级别 / 正文
    #[test]
    fn payload_holds_fields() {
        let msg = PluginNoticeMessage::new("loop_guard", NoticeLevel::Warn, "检测到循环");
        assert_eq!(msg.payload.source.name, "loop_guard");
        assert_eq!(msg.payload.level, NoticeLevel::Warn);
        assert_eq!(msg.payload.content, "检测到循环");
    }

    /// 级别三级齐全且可判等
    #[test]
    fn level_variants_compare() {
        assert_ne!(NoticeLevel::Info, NoticeLevel::Warn);
        assert_ne!(NoticeLevel::Warn, NoticeLevel::Error);
        assert_eq!(NoticeLevel::Error, NoticeLevel::Error);
    }

    /// clone 后字段一致
    #[test]
    fn clone_keeps_fields() {
        let msg = PluginNoticeMessage::new("loop_guard", NoticeLevel::Error, "已终止");
        let cloned = msg.clone();
        assert_eq!(cloned.payload.source.name, msg.payload.source.name);
        assert_eq!(cloned.payload.level, msg.payload.level);
        assert_eq!(cloned.payload.content, msg.payload.content);
    }

    /// JSON 序列化 round-trip：字段不丢、级别序列化为变体名
    #[test]
    fn serde_roundtrip() {
        let msg = PluginNoticeMessage::new("loop_guard", NoticeLevel::Warn, "重复执行");
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: PluginNoticeMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.payload.source.name, "loop_guard");
        assert_eq!(parsed.payload.level, NoticeLevel::Warn);
        assert_eq!(parsed.payload.content, "重复执行");
    }
}
