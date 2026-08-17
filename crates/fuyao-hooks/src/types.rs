//! 钩子类型定义
//!
//! 钩子函数签名类型别名：拦截钩子（原地 mutate 协议）+ 观察钩子（Arc 共享只读协议）。

use fuyao_api::message::OutputEvent;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// 输出拦截钩子：原地修改事件，返回是否阻止
///
/// 签名协议设计意图：钩子拿到 `&mut` 事件**原地修改**——不修改的钩子零开销
/// （纯借用），修改的钩子无需 clone 往返（返回值式协议下每层钩子都要整事件
/// clone 一次，热路径 clone 税高，原地借用将其消除）。
///
/// 返回值语义与修改语义显式分离：
/// - `None`：通过（事件保留钩子已做的原地修改，继续走下一个钩子）
/// - `Some(reason)`：阻止当前事件（携带原因），执行链立即短路
///
/// 拦截只负责修改或阻止事件，不承载中断职责。
/// 中断统一通过 [`SessionSender::send_interrupt`](crate::SessionSender::send_interrupt) 发送，
/// 由引擎主循环处理。
pub type OutputInterceptFn = Arc<dyn Fn(&mut OutputEvent) -> Option<String> + Send + Sync>;

/// 输出观察钩子：异步副作用，共享只读事件
///
/// 事件以 [`Arc`] 传入——同一事件分发给多个观察钩子时，每个钩子只做一次
/// 引用计数拷贝（廉价），事件本体全程零 clone，多观察钩子共享同一份只读数据。
/// 需要 owned 数据的钩子自行解引用后 clone 所需字段。
pub type OutputObserveFn =
    Arc<dyn Fn(Arc<OutputEvent>) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;
