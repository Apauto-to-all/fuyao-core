//! 钩子类型定义
//!
//! 拦截结果 + 钩子函数签名类型别名。

use fuyao_api::Message;
use fuyao_api::message::{InputEvent, OutputEvent};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// 拦截钩子返回值：通过或阻止
///
/// 拦截只负责修改或阻止事件，不承载中断职责。
/// 中断统一通过 InputEvent::Interrupt 发送，由引擎主循环处理。
#[derive(Debug)]
pub enum InterceptResult<T> {
    /// 通过，携带值（可能被修改）
    Pass(T),
    /// 阻止当前事件，携带原因
    Block(String),
}

/// before_llm 钩子输出
///
/// 包含消息列表和工具控制信号。
/// `skip_tools` 为 true 时，引擎不传工具给 LLM（压缩轮次等场景）。
#[derive(Debug, Default)]
#[must_use]
pub struct BeforeLlmOutput {
    /// 消息列表
    pub messages: Vec<Message>,
    /// 是否跳过工具传递给 LLM
    pub skip_tools: bool,
}

/// before_llm 钩子：异步，返回 BeforeLlmOutput
pub type BeforeLlmFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = BeforeLlmOutput> + Send>> + Send + Sync>;

/// 输出拦截钩子：可修改或阻止事件
pub type OutputInterceptFn =
    Arc<dyn Fn(&OutputEvent) -> InterceptResult<OutputEvent> + Send + Sync>;

/// 输出观察钩子：异步副作用
pub type OutputObserveFn =
    Arc<dyn Fn(OutputEvent) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// LLM 错误处理动作
#[derive(Debug, Clone)]
pub enum LlmErrorAction {
    /// 继续重试当前模型（默认）
    Retry,
    /// 退回到指定模型
    Fallback(String),
    /// 放弃，终止轮次
    Abort,
}

/// LLM 错误决策钩子：同步，返回处理动作
pub type OnLlmErrorFn = Arc<dyn Fn(&str, u32) -> LlmErrorAction + Send + Sync>;

/// 发送输入事件钩子：真·主动，插件持有 Sender 可随时发送
///
/// 引擎在初始化时调用一次，传入 InputEvent 的 Sender。
/// 插件保存此 Sender，后续在任何时机（不依赖 emit 频率）
/// 调用 try_send() 发送任意 InputEvent。
///
/// 支持所有 InputEvent 类型：
/// - User: 注入用户消息（引导 AI、布置任务）
/// - Interrupt: 请求中断当前轮次
/// - Shutdown: 请求关闭引擎
/// - 未来新增的任何输入事件类型
///
/// 这是真·主动：插件自主决定何时发送，引擎只负责消费。
pub type SendInputFn = Arc<
    dyn Fn(tokio::sync::mpsc::Sender<InputEvent>) -> Pin<Box<dyn Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn before_llm_output_default() {
        let output = BeforeLlmOutput::default();
        assert!(output.messages.is_empty());
        assert!(!output.skip_tools);
    }
}
