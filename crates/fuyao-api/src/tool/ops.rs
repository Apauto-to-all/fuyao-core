//! 工具运行期能力注入接口
//!
//! 定义两类「工具经 [`crate::ToolCallContext`] 持有、运行时注入」的最小能力接口：
//! - [`SubagentOps`]：派生子 session 的引擎能力（弱引用，引擎可能已关闭）
//! - [`TodoStoreOps`]：读写任务列表的存储能力（强引用，随引擎生命周期存活）
//!
//! 这种「不直接捕获引擎/存储、由调用方运行时通过 ctx 注入」的模式解耦了
//! 工具注册表与引擎/存储的构造顺序——工具 handler 不在闭包里持有这些句柄，
//! 能力在工具派发入口构造 ctx 时注入。

use std::future::Future;
use std::pin::Pin;

use tokio::sync::mpsc;

use crate::{AgentConfig, InputEvent, OutputEvent, TodoItem};

/// 子任务 session 的上下文来源
///
/// 决定新子任务 session 是空上下文起步，还是 fork 某个源 session 的可见上下文。
#[derive(Debug, Clone)]
pub enum ChildSessionSource {
    /// 全新创建：空上下文，system_prompt 从 agent_config 构建
    Fresh,
    /// fork 旧 session：复制 source_id 的可见消息 + system_prompt
    Fork(String),
}

/// 引擎派生子 session 的最小能力接口
///
/// 工具 handler（如子代理工具）通过 [`crate::ToolCallContext`] 持有引擎弱引用
/// (`Weak<dyn SubagentOps>`)，调用时 upgrade 后调本 trait 方法派生子任务 session。
///
/// 返回 `Result<T, String>`：失败时返错误描述字符串。调用方据此生成可读工具结果
/// 回喂父 ReAct，不需要按错误类型分支。
//
// trait 方法返 `Pin<Box<dyn Future + Send + 'a>>` 是 async fn in dyn trait 的标准写法，
// 类型复杂度高但无简洁等价物（Rust 2024 async fn in dyn trait 尚未稳定到此场景）
#[allow(clippy::type_complexity)]
pub trait SubagentOps: Send + Sync {
    /// 创建子任务 session，返 `(id, rx)`——rx 由调用方独占消费
    fn create_child_session<'a>(
        &'a self,
        parent_session_id: &'a str,
        source: ChildSessionSource,
        child_agent_config: AgentConfig,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(String, mpsc::UnboundedReceiver<OutputEvent>), String>>
                + Send
                + 'a,
        >,
    >;

    /// 向指定 session 发消息
    fn send<'a>(
        &'a self,
        id: &'a str,
        event: InputEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

    /// 销毁指定 session（带原因）
    fn end_session<'a>(
        &'a self,
        id: &'a str,
        end_reason: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

/// 任务列表（todo）存储的最小能力接口
///
/// 工具 handler（如 todo 工具）通过 [`crate::ToolCallContext`] 持有存储强引用
/// (`Arc<dyn TodoStoreOps>`)，调用时直接调本 trait 方法读写指定 session 的任务列表。
///
/// 实现方是 `SessionStore`（会话存储是 `sessions.db` 的唯一 owner，任务列表表
/// 与会话表同居一个数据库），由装配层创建并注入引擎，随引擎生命周期存活——
/// 故用 `Arc` 而非 `Weak`（无「引擎已关闭」的降级语义）。
///
/// 返回 `Result<T, String>`：失败时返错误描述字符串。调用方据此生成可读工具结果
/// 回喂 ReAct，不需要按错误类型分支。
//
// trait 方法返 `Pin<Box<dyn Future + Send + 'a>>` 是 async fn in dyn trait 的标准写法，
// 类型复杂度高但无简洁等价物（Rust 2024 async fn in dyn trait 尚未稳定到此场景）
#[allow(clippy::type_complexity)]
pub trait TodoStoreOps: Send + Sync {
    /// 读取指定 session 的任务列表（按 sort_order 排序）
    fn read_todos<'a>(
        &'a self,
        session_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<TodoItem>, String>> + Send + 'a>>;

    /// 整体覆盖写入指定 session 的任务列表
    ///
    /// 先删该 session 全部任务，再按传入顺序重新插入。
    fn write_todos<'a>(
        &'a self,
        session_id: &'a str,
        todos: Vec<TodoItem>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<TodoItem>, String>> + Send + 'a>>;
}
