//! 钩子类型定义
//!
//! 拦截结果 + 钩子函数签名类型别名。

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

/// 输出拦截钩子：可修改或阻止事件
pub type OutputInterceptFn =
    Arc<dyn Fn(&OutputEvent) -> InterceptResult<OutputEvent> + Send + Sync>;

/// 输出观察钩子：异步副作用
pub type OutputObserveFn =
    Arc<dyn Fn(OutputEvent) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// 发送输入事件钩子：真·主动，插件持有 Sender 可随时发送
///
/// 引擎在初始化时调用一次，传入 InputEvent 的 Sender。
/// 插件保存此 Sender，后续在任何时机（不依赖 emit 频率）
/// 调用 try_send() 发送任意 InputEvent。
///
/// 支持所有 InputEvent 类型：
/// - User: 注入用户消息（引导 AI、布置任务）
/// - Interrupt: 请求中断当前轮次
/// - 未来新增的任何输入事件类型
///
/// 这是真·主动：插件自主决定何时发送，引擎只负责消费。
pub type SendInputFn = Arc<
    dyn Fn(tokio::sync::mpsc::Sender<InputEvent>) -> Pin<Box<dyn Future<Output = ()> + Send>>
        + Send
        + Sync,
>;
