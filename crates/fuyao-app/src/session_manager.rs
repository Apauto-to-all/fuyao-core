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

use fuyao_api::OutputEvent;
use fuyao_api::{Message, Session};
use fuyao_session::SessionStore;

use crate::history_replay;

/// 会话管理器：持有会话存储句柄，对外提供会话 / 消息的查询接口
///
/// 与 [`App`](crate::App) 平级正交：
/// - [`App`](crate::App) 管「对话的进行」（create / send / recv）
/// - `SessionManager` 管「会话的检索与浏览」（列会话 / 查历史）
///
/// 两者共享同一份 `SessionStore`（Arc 克隆，零拷贝共享连接池）。
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

    /// 默认分页大小（每页消息条数）
    ///
    /// 向上滚动加载历史的常见档位：既不因每页过少而频繁请求，也不因过多撑爆渲染。
    /// 调用方可经 `limit` 参数覆盖此默认值。
    const DEFAULT_MESSAGE_PAGE_SIZE: i64 = 50;

    /// 游标分页加载历史消息（给人看的历史浏览，seq 倒序）
    ///
    /// 打开会话先看最新一页，向上滚动加载更早消息。与给 LLM 构造请求的可见窗口
    /// （`SessionStore::load_visible_messages`）是正交两条路径，互不影响。
    ///
    /// # 游标分页（不用 OFFSET）
    ///
    /// 消息是持续追加的流，OFFSET 基于「位置」分页，新消息插入会让整页内容向后漂移、
    /// 重复或遗漏。本接口基于消息的稳定标识 `seq`（单调递增、插入后永不改）做游标分页：
    ///
    /// - `before_seq = None`：从最新一条开始（第一页）
    /// - `before_seq = Some(N)`：取 `seq < N` 的更早一页，锚点本身不含
    ///
    /// 「有没有下一页」用返回条数 == `limit` 判断，不提供总数。
    ///
    /// # 参数
    ///
    /// - `session_id`：会话 ID
    /// - `before_seq`：游标锚点。`None` 取第一页（最新），`Some(N)` 向前翻（取 seq < N）
    /// - `limit`：每页条数。`None` 用 [`DEFAULT_MESSAGE_PAGE_SIZE`](Self::DEFAULT_MESSAGE_PAGE_SIZE)
    ///
    /// # 压缩消息处理
    ///
    /// compaction 消息（摘要）当作对话流里的一个普通节点正常显示，不过滤——
    /// 全部消息（普通 + 压缩）按 seq 倒序一起分页。
    pub async fn list_messages(
        &self,
        session_id: &str,
        before_seq: Option<i64>,
        limit: Option<i64>,
    ) -> Result<Vec<Message>, fuyao_session::SessionError> {
        let limit = limit.unwrap_or(Self::DEFAULT_MESSAGE_PAGE_SIZE).max(1);
        self.store
            .list_messages_before(session_id, before_seq, limit)
            .await
    }

    /// 历史消息投影成事件流（历史回放，seq 正序）
    ///
    /// 与 [`list_messages`](Self::list_messages) 同源取数（同游标、同分页），但把存储
    /// [`Message`] 投影成与实时流同构的 [`OutputEvent`]——前端历史回放与实时流共用一套
    /// 渲染逻辑，无需区分数据来源。转换由 [`history_replay`] 承担（含 tool_calls 嵌套 →
    /// 扁平的逆向、seq 倒序翻正序），详见该模块。
    ///
    /// # 参数
    /// 同 [`list_messages`](Self::list_messages)。
    pub async fn list_events(
        &self,
        session_id: &str,
        before_seq: Option<i64>,
        limit: Option<i64>,
    ) -> Result<Vec<OutputEvent>, fuyao_session::SessionError> {
        let messages = self.list_messages(session_id, before_seq, limit).await?;
        Ok(history_replay::messages_to_events(messages))
    }
}
