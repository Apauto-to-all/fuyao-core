//! 控制命令枚举
//!
//! 控制命令消息（input / output 侧 `ControlMessage`）的载荷本体。
//! 后续新增控制类功能 = 给 [`ControlCommand`] 加变体，
//! 不加新消息、不加新输入事件、不开新通道。

/// 控制命令：在队列消费时机执行的指令
///
/// 每个变体代表一种指令。命令经
/// [`crate::message::input::InputEvent::Control`] 从外部送入
/// （serde 序列化随消息过 IPC），也可由内核组件直接构造。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ControlCommand {
    /// 手动触发上下文压缩（跳过阈值 / 反抖动，触发原因标记为 manual）
    Compress,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 命令 serde 往返（随消息过 IPC）
    #[test]
    fn control_command_serde_roundtrip() {
        let json = serde_json::to_string(&ControlCommand::Compress).expect("序列化失败");
        let de: ControlCommand = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(de, ControlCommand::Compress);
    }
}
