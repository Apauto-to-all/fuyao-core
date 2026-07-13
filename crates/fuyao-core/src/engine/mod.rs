//! 引擎核心
//!
//! 两层分离的引擎层：启动一次，装配能力（provider / store / 出口通道）；
//! 多个对话按需创建，各自独立跑交互。

mod types;

use crate::error::EngineError;
use fuyao_api::{EngineParams, InputEvent, MessageParams, OutputEvent, Session, SessionParams};
use fuyao_provider::Provider;
use fuyao_session::SessionStore;
pub use types::SessionId;

/// 引擎
///
/// 能力共享层：构造时装配一次，持有 DB 句柄、provider、出口通道等引擎级共享。
/// 多个对话（Session）共享同一个 Engine 实例，靠 session id 区分。
///
/// 公开 API 遵循设计文档的四个动作：
/// - [`new`](Self::new)：启动引擎（构造即启动）
/// - [`create_session`](Self::create_session)：创建对话
/// - [`resume_session`](Self::resume_session)：恢复对话
/// - [`send`](Self::send)：入事件（单一入口）
/// - [`recv`](Self::recv)：出事件（单一出口）
/// - [`shutdown`](Self::shutdown)：关闭引擎（独立方法，不走消息流）
#[allow(dead_code)]
pub struct Engine {
    /// 会话存储层（引擎持有 DB 句柄，所有 session 共享同一连接池）
    store: SessionStore,

    /// LLM 提供者（引擎级共享，所有 session 用同一个 provider 实例）
    provider: Box<dyn Provider>,

    /// 活跃 session 调度表（session_id → 内存态 Session）
    ///
    /// 活跃 session 的历史在内存；不活跃的只在数据库。
    /// 创建/恢复时进入此表，结束后移除（移除时机后续补）。
    sessions: std::collections::HashMap<SessionId, Session>,

    /// 事件出口通道（单一出口）
    ///
    /// 所有 session 的产出事件从此通道流出，每条事件带 session_id 标签。
    tx_event: tokio::sync::mpsc::Sender<OutputEvent>,
}

impl Engine {
    /// 启动引擎（动作一）
    ///
    /// 构造即启动：装配 provider、持有 DB 句柄、建立出口通道。
    /// 启动完成后才可创建/恢复对话。
    ///
    /// 当前为骨架，内部逻辑后续填充。
    #[allow(clippy::new_ret_no_self)]
    pub fn new(_params: EngineParams, _provider: Box<dyn Provider>) -> Self {
        todo!("引擎启动：装配 store（用 params.agent_paths 解析 db_path）+ 建出口通道")
    }

    /// 创建对话（动作二）
    ///
    /// 从零创建一个新 Session：生成编号、登记进调度表。
    /// `SessionParams` 创建时定死且不可变（Agent 配置改了会冲掉前缀缓存）。
    ///
    /// 返回新 session id。
    pub async fn create_session(&self, _params: SessionParams) -> Result<SessionId, EngineError> {
        todo!("创建新 Session：生成 id + 构建系统提示词 + 登记进调度表 + 落库")
    }

    /// 恢复对话（动作三）
    ///
    /// 把数据库里的老对话捞回内存：用 session id 从 store 加载历史，
    /// 装进内存，重新登记进调度表。
    ///
    /// session id 不在数据库 → 同步返回 `Err(SessionNotFound)`。
    pub async fn resume_session(&self, _id: &SessionId) -> Result<(), EngineError> {
        todo!("恢复 Session：从 store 加载历史 + 装进内存 + 登记进调度表")
    }

    /// 入事件（单一入口）
    ///
    /// 所有对话级输入事件从一个口进，靠 session id 区分对话，按事件类型分流：
    /// - `User`：入队，触发 ReAct 循环。`MessageParams` 决定本轮用哪个模型
    /// - `Interrupt`：发出中断信号，打断对应对话的当前执行
    /// - `Plugin`：插件发给某对话的通知，转发为 OutputEvent::Plugin 送出
    ///
    /// 入队即返回，不阻塞——不等大模型想完。
    /// 后续产出从 [`recv`](Self::recv) 流出。
    ///
    /// session id 不在调度表 → 同步返回 `Err(SessionNotFound)`（要恢复走恢复动作）。
    ///
    /// `MessageParams` 决定本轮用哪个模型、怎么思考——model id 跟着消息走。
    /// 仅 `User` 变体使用，其他变体忽略此参数。
    pub async fn send(
        &self,
        _id: &SessionId,
        _event: InputEvent,
        _params: MessageParams,
    ) -> Result<(), EngineError> {
        todo!("入事件：校验 session id 在调度表 → 按变体分流（入队/中断/转发）")
    }

    /// 关闭引擎
    ///
    /// 引擎关闭是危险操作，不混入对话级的事件流（不走 send），
    /// 由独立的关闭方法触发。
    ///
    /// 关闭流程（后续实现）：
    /// - 停止接收新的对话级事件
    /// - 等待所有活跃 session 的当前执行完成或优雅中断
    /// - 落库未持久化的状态
    /// - 关闭 DB 连接、释放资源
    // TODO: 实现引擎关闭流程（停止调度 + 落库 + 释放资源）
    pub async fn shutdown(&self) {
        todo!("引擎关闭：停止调度 + 落库 + 释放资源")
    }

    /// 出事件（单一出口）
    ///
    /// 从统一出口取下一条产出事件，按 session_id 归类到对应对话。
    /// 所有对话的产出都从此口流出，没有第二个出口。
    ///
    /// 返回 `None` 表示引擎已关闭、通道已断。
    pub async fn recv(&self) -> Option<OutputEvent> {
        todo!("从出口通道取下一条事件")
    }
}
