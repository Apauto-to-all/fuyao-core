//! 会话管理门面：会话与消息的查询接口
//!
//! 与 [`crate::App`]（运行时交互门面）平级正交，专司「会话的检索与浏览」：
//! - [`crate::App`] 管对话的进行（create / send / recv）
//! - [`SessionManager`] 管会话的检索与浏览（列会话 / 查历史）
//!
//! 两者共享同一份 [`fuyao_session::SessionStore`]（由装配层 [`crate::start`] 注入
//! `Arc` 克隆），各取所需：Engine 写（LLM 流程落库）、SessionManager 读（查询 / 浏览）。
//! 通讯方式为直接异步方法调用——查询是纯存储读、不涉及 LLM、不需要流式产出，
//! 与 `App::create_session` 返 `SessionId` 同属「管理型同步方法」，不走消息总线。
//!
//! # 设计依据
//! 详见 `docs/开发/设计文档/01-会话查询接口设计.md`。

use std::sync::Arc;

use fuyao_api::{Message, Session};
use fuyao_session::SessionStore;

/// 会话管理器：持有会话存储句柄，对外提供会话 / 消息的查询接口
///
/// 与 [`App`](crate::App) 平级正交：
/// - [`App`](crate::App) 管「对话的进行」（create / send / recv）
/// - `SessionManager` 管「会话的检索与浏览」（列会话 / 查历史）
///
/// 两者共享同一份 `SessionStore`（Arc 克隆，零拷贝共享连接池）。
///
/// 查询接口目前为占位（方法体 `todo!()`），分页语义、返回结构等细节待后续设计落实。
pub struct SessionManager {
    /// 会话存储句柄（与 Engine 共享同一份，Arc 克隆）
    store: Arc<SessionStore>,
}

impl SessionManager {
    /// 由装配层（[`crate::start`]）注入 store 句柄构造
    ///
    /// 与 Engine 共享同一份 `SessionStore`——传入的是 `Arc` 克隆，仅增引用计数、零拷贝，
    /// 两者指向同一份内存、同一个 `SqlitePool` 连接池。
    pub fn new(store: Arc<SessionStore>) -> Self {
        Self { store }
    }

    // ── 会话查询 ───────────────────────────────────────────────

    /// 列举历史会话（分页，按最近活动时间倒序）
    ///
    /// 返回的会话按 `last_active_at` 倒序——用户刚交互的会话排最前。
    ///
    /// # 参数
    /// - `workspace_filter`：传 `Some(path)` 只列该工作目录的会话；`None` 列全部（含无 workspace 的）
    /// - `limit` / `offset`：分页，单页条数与偏移量
    pub async fn list_sessions(
        &self,
        workspace_filter: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Session>, fuyao_session::SessionError> {
        self.store.list_all(workspace_filter, limit, offset).await
    }

    /// 会话总数（可选按工作目录过滤）
    ///
    /// 配合 [`list_sessions`](Self::list_sessions) 的分页，供上层计算总页数。
    /// `workspace_filter` 须与 `list_sessions` 传的一致，否则总数与列表对不上。
    pub async fn session_count(
        &self,
        workspace_filter: Option<&str>,
    ) -> Result<i64, fuyao_session::SessionError> {
        self.store.count_with_filter(workspace_filter).await
    }

    // ── 消息查询 ───────────────────────────────────────────────
    // 接口占位：分页语义、排序方向、窗口口径待后续设计落实。

    /// 获取指定会话的最近消息（给人看的，分页）
    ///
    /// 加载某个会话的历史消息供用户浏览。分页参数语义、排序方向、与压缩窗口的口径关系
    /// 待后续设计落实。
    pub async fn list_messages(&self, session_id: &str) -> Vec<Message> {
        let _ = (&self.store, session_id);
        todo!("分页加载会话历史消息：待设计落实分页参数、排序方向与窗口口径")
    }

    /// 指定会话的消息总数
    ///
    /// 配合 [`list_messages`](Self::list_messages) 的分页，供上层计算总页数。
    pub async fn message_count(&self, session_id: &str) -> i64 {
        let _ = (&self.store, session_id);
        todo!("统计指定会话的消息总数：待设计落实")
    }
}
