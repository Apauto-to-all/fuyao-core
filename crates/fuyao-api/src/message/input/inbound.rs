//! 入站用户消息载荷
//!
//! 引擎层把外部输入事件 `InputEvent::User` 解包后的内部载荷结构体。
//! 供 session 入站通道传递，由 session task 在 select! 收到后过完整管道
//! （拦截 → 处理[入队] → 发送[回显] → 观察）。

use crate::MessageParams;
use crate::UserMessageMode;

/// 入站用户消息（引擎层送进 session task 的载荷）
///
/// 当 [`Engine::send`](crate::InputEvent::User) 收到 `InputEvent::User` 后，
/// 提取 payload 转成此结构，发到 session 的入站通道。session task 收到后
/// 按管道四段处理（拦截/处理/发送/观察），其中处理段按 `mode` 入 guide 或 pending 队列。
///
/// 与 `InputEvent::User` 的关系：
/// - `InputEvent::User` 是外部输入契约（含 base 元信息），协议对外稳定
/// - `InboundUser` 是引擎内部载荷（去掉 base，只留业务字段），随引擎演进
///
/// 定义在 `fuyao-api` 是为了让 `fuyao-hooks` 的 `SessionSender` 能持有
/// `Sender<InboundUser>`，避免 hooks 反向依赖 `fuyao-core`（hooks 是 L1，
/// core 是 L3）。
#[derive(Debug, Clone)]
pub struct InboundUser {
    /// 消息文本
    pub content: String,
    /// 消息模式（Guide / Pending），决定入哪个队列
    pub mode: UserMessageMode,
    /// 消息参数（model id 等，跟着每条消息走）
    pub params: MessageParams,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_user_holds_fields() {
        let params = MessageParams::default();
        let inbound = InboundUser {
            content: "你好".into(),
            mode: UserMessageMode::Guide,
            params: params.clone(),
        };
        assert_eq!(inbound.content, "你好");
        assert_eq!(inbound.mode, UserMessageMode::Guide);
        assert_eq!(
            inbound.params.model_config.model_id,
            params.model_config.model_id
        );
    }

    #[test]
    fn inbound_user_clone_works() {
        let inbound = InboundUser {
            content: "clone 测试".into(),
            mode: UserMessageMode::Pending,
            params: MessageParams::default(),
        };
        let cloned = inbound.clone();
        assert_eq!(inbound.content, cloned.content);
        assert_eq!(inbound.mode, cloned.mode);
    }
}
