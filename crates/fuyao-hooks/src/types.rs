//! 钩子类型定义
//!
//! 拦截结果 + 钩子函数签名类型别名。

use fuyao_api::message::OutputEvent;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// 拦截钩子返回值：通过或阻止
///
/// 拦截只负责修改或阻止事件，不承载中断职责。
/// 中断统一通过 [`SessionSender::send_interrupt`](crate::SessionSender::send_interrupt) 发送，
/// 由引擎主循环处理。
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
