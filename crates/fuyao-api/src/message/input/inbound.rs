//! 入站用户消息载荷
//!
//! 引擎层把外部输入事件 `InputEvent::User` 立即转化为输出侧 `OutputUserMessage`
//! （字段完整照搬，含 source），与 `MessageParams` 一起打包成本结构，
//! 经 session 入站通道送进 session task。
//!
//! session task 收到后由 `handle_inbound_user` **纯入队**（无任何 side effect），
//! 等到 `inject_messages` 消费时才统一过 `emit_to_history` 管道
//! （拦截 → push → 发送 → 观察）。

use crate::MessageParams;
use crate::message::output::UserMessage as OutputUserMessage;

/// 入站用户消息（引擎层送进 session task 的载荷）
///
/// 当 [`Engine::send`](crate::InputEvent::User) 收到 `InputEvent::User` 后，
/// 立即把 input 侧的 `UserMessage` 字段照搬转化为 output 侧 `OutputUserMessage`
/// （base + payload 完整保留，含 source），与 `MessageParams` 一起打包，
/// 发到 session 的入站通道。
///
/// session task 收到后**纯入队**（无拦截、无发送、无观察），所有处理推迟到
/// `inject_messages` 消费时统一过 `emit_to_history` 管道——与 assistant / tool_result
/// 走完全相同的路径。
///
/// 与 `InputEvent::User` 的关系：
/// - `InputEvent::User` 是外部输入契约（input 侧 UserMessage），协议对外稳定
/// - `InboundUser` 是引擎内部载荷（output 侧 UserMessage + params），随引擎演进
///
/// 定义在 `fuyao-api` 是为了让 `fuyao-hooks` 的 `SessionSender` 能持有
/// `Sender<InboundUser>`，避免 hooks 反向依赖 `fuyao-core`（hooks 是 L1，
/// core 是 L3）。
#[derive(Debug, Clone)]
pub struct InboundUser {
    /// 用户输出消息（已从 input 侧字段照搬转化，含 content + mode + source）
    ///
    /// 携带完整的 output 侧 `UserMessage`（base + payload），保证 source 等字段
    /// 完整流到消费时刻。base.session_id 由 Emitter 在 deliver 时盖标签。
    pub message: OutputUserMessage,
    /// 消息参数（model id 等，跟着每条消息走）
    pub params: MessageParams,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::EventBase;
    use crate::message::input::{UserMessageMode, UserMessageSource};

    #[test]
    fn inbound_user_holds_fields() {
        let params = MessageParams::default();
        let message = OutputUserMessage {
            base: EventBase::default(),
            payload: crate::message::output::UserPayload {
                content: "你好".into(),
                mode: UserMessageMode::Guide,
                source: UserMessageSource::User,
            },
        };
        let inbound = InboundUser {
            message: message.clone(),
            params: params.clone(),
        };
        assert_eq!(inbound.message.payload.content, "你好");
        assert_eq!(inbound.message.payload.mode, UserMessageMode::Guide);
        assert_eq!(inbound.message.payload.source, UserMessageSource::User);
        assert_eq!(
            inbound.params.model_config.model_id,
            params.model_config.model_id
        );
    }

    #[test]
    fn inbound_user_preserves_plugin_source() {
        // 插件注入的消息应保留 Plugin 来源标识（核心修复点：source 不能丢）
        let message = OutputUserMessage {
            base: EventBase::default(),
            payload: crate::message::output::UserPayload {
                content: "循环检测提醒".into(),
                mode: UserMessageMode::Guide,
                source: UserMessageSource::Plugin(crate::message::input::PluginSource {
                    name: "loop_guard".into(),
                }),
            },
        };
        let inbound = InboundUser {
            message,
            params: MessageParams::default(),
        };
        match inbound.message.payload.source {
            UserMessageSource::Plugin(p) => assert_eq!(p.name, "loop_guard"),
            _ => panic!("source 应为 Plugin"),
        }
    }

    #[test]
    fn inbound_user_clone_works() {
        let inbound = InboundUser {
            message: OutputUserMessage {
                base: EventBase::default(),
                payload: crate::message::output::UserPayload {
                    content: "clone 测试".into(),
                    mode: UserMessageMode::Pending,
                    source: UserMessageSource::User,
                },
            },
            params: MessageParams::default(),
        };
        let cloned = inbound.clone();
        assert_eq!(
            inbound.message.payload.content,
            cloned.message.payload.content
        );
        assert_eq!(inbound.message.payload.mode, cloned.message.payload.mode);
    }
}
