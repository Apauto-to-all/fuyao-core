//! 会话管理门面：会话检索 / 浏览 / 元数据编辑的接口
//!
//! 与 [`crate::App`]（运行时交互门面）平级正交：
//! - [`crate::App`] 管对话的进行（create / send / recv）
//! - [`SessionManager`] 管会话的检索 / 浏览 / 手动编辑（列会话 / 查历史 / 改标题）
//!
//! 两者共享同一份 [`fuyao_session::SessionStore`]（由装配层 [`crate::start`] 注入
//! `Arc` 克隆），各取所需：Engine 写（LLM 流程落库、自动标题生成）、SessionManager
//! 读查询 + 补引擎不做的「用户 / 应用手动编辑」写（如手改标题、回退、派生分支）。
//! 通讯方式为直接异步方法调用——纯存储操作、不涉及 LLM、不需要流式产出，
//! 与 `App::create_session` 返 `SessionId` 同属「管理型同步方法」，不走消息总线。
//!
//! # 写能力边界
//!
//! 本门面只暴露**适合外部编辑**的字段。引擎内核的自动写（create / 压缩后重建
//! system_prompt / 标题异步生成 / destroy_session）是其 ReAct 循环与 session task
//! 生命周期的内生产物，不在此暴露——外部插手会破坏一致性。
//!

use std::sync::Arc;

use fuyao_session::SessionStore;

/// 会话管理器：持有会话存储句柄，对外提供会话检索 / 浏览 / 元数据编辑接口
///
/// 与 [`App`](crate::App) 平级正交：
/// - [`App`](crate::App) 管「对话的进行」（create / send / recv）
/// - `SessionManager` 管「会话的检索 / 浏览 / 手动编辑」（列会话 / 查历史 / 改标题）
///
/// 两者共享同一份 `SessionStore`（Arc 克隆，零拷贝共享连接池）。
///
/// `Clone` 廉价：唯一字段是 `Arc<SessionStore>`，clone 仅增引用计数、零拷贝，
/// 两个 clone 共享同一份存储与连接池。供消费方（如适配层在锁内 clone 出 owned
/// 句柄以消除借用穿透 await）按需取用。
#[derive(Clone)]
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

    /// 列举历史会话（分页，按最近活动时间倒序，只含主会话）
    ///
    /// 返回的会话按 `last_active_at` 倒序——用户刚交互的会话排最前。
    /// 只返回顶层会话（`parent_session_id` 为空）；子会话经
    /// [`list_child_sessions`](Self::list_child_sessions) 按父列举。`total` 与
    /// `items` 同语义（只计主会话）。
    ///
    /// # 参数
    /// - `workspace_filter`：传 `Some(path)` 只列该工作目录的会话；`None` 列全部（含无 workspace 的）
    /// - `limit` / `offset`：分页，单页条数与偏移量
    pub async fn list_sessions(
        &self,
        workspace_filter: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<fuyao_api::SessionPage, fuyao_session::SessionError> {
        // items 与 total 是两条独立查询（list + count），同一 workspace_filter。
        // 极端情况下两次查询之间有新会话插入会致二者差一两条——对「列表分页给 UI 算页数」
        // 是可接受的弱一致（过时的 1-2 条不影响翻页体验）。
        let items = self.store.list_all(workspace_filter, limit, offset).await?;
        let total = self.store.count_with_filter(workspace_filter).await?;
        Ok(fuyao_api::SessionPage {
            items,
            total,
            limit,
            offset,
        })
    }

    /// 列举主会话下的全部子会话（全量、不分页、按创建序）
    ///
    /// 透传 [`SessionStore::list_child_sessions`](fuyao_session::SessionStore::list_child_sessions)：
    /// 返回该父会话派生的全部子会话（子代理 / 后台任务），按 `started_at` 升序（先派生的
    /// 排前面）。全量返回、无分页信封——子会话数由单次任务派生的子代理数决定，天然有限。
    ///
    /// 行的 `parent_session_id` 恒指向 `parent_id`；父不存在或无子会话时返回空列表。
    ///
    /// # 参数
    /// - `parent_id`：父会话（主会话）id
    pub async fn list_child_sessions(
        &self,
        parent_id: &str,
    ) -> Result<Vec<fuyao_api::Session>, fuyao_session::SessionError> {
        self.store.list_child_sessions(parent_id).await
    }

    /// 获取单个会话的最新元数据（纯元数据，不含消息）
    ///
    /// 单行主键直读，返回 DB 当前值——token 累计四项与 `total_cost`（落库 assistant
    /// 消息时由 `insert_message` 事务内原子累加）、`last_active_at`（消息落库时同
    /// 事务刷新）均为 DB 派生字段。落库先于事件发送，消费方在收到事件的时点经此
    /// 拉取即可拿到含该事件效果的最新读数——以 DB 为唯一真相源对齐内存快照，无需
    /// 在应用侧复刻累计规则。
    ///
    /// # 返回
    /// - `Ok(Some(session))`：命中
    /// - `Ok(None)`：session_id 在数据库中不存在
    pub async fn get_session(
        &self,
        session_id: &str,
    ) -> Result<Option<fuyao_api::Session>, fuyao_session::SessionError> {
        self.store.get(session_id).await
    }

    // ── 会话元数据编辑（面向二次开发应用）──────────────────────

    /// 更新会话标题
    ///
    /// 供二次开发应用手动改名（如 TUI 里用户重命名会话、CLI 批量改标题）。与引擎的
    /// 自动标题生成（`react/turn.rs` fire-and-forget spawn）正交：引擎只产出默认标题，
    /// 本方法供应用 / 用户覆盖；两者最终都落同一个局部 UPDATE，最后一个写生效，单字段原子。
    ///
    /// 透传 [`SessionStore::update_session`](fuyao_session::SessionStore::update_session)：
    /// EXISTS 校验后局部 UPDATE title，不动其他字段、不动 messages 表。
    ///
    /// # 返回
    /// - `Ok(())`：标题已更新
    /// - `Err(SessionError::NotFound)`：session_id 在数据库中不存在
    pub async fn update_title(
        &self,
        session_id: &str,
        new_title: &str,
    ) -> Result<(), fuyao_session::SessionError> {
        self.store
            .update_session(session_id, Some(new_title), None)
            .await
    }

    // ── 会话删除 ───────────────────────────────────────────────

    /// 删除会话（cascade 删该会话及其全部子会话，含各自的消息 + 任务列表）
    ///
    /// 透传 [`SessionStore::delete`](fuyao_session::SessionStore::delete)：单事务内删
    /// todos + messages + sessions，三者要么全删要么全留。
    ///
    /// # 返回
    /// - `Ok(true)`：会话存在，删除成功
    /// - `Ok(false)`：会话不存在（无 session 行被删，但该 id 组内的子会话与残留 todos / messages 仍被清理）
    pub async fn delete_session(
        &self,
        session_id: &str,
    ) -> Result<bool, fuyao_session::SessionError> {
        self.store.delete(session_id).await
    }

    // ── 会话回退 ───────────────────────────────────────────────

    /// 把会话回退到目标消息之前（删目标消息及其后的所有消息 + 重算 count 类与压缩元数据）
    ///
    /// 复用存储层单事务原子执行体 [`SessionStore::rollback_to`](fuyao_session::SessionStore::rollback_to)
    /// （删消息 + 重算 + 局部 UPDATE），执行成功返回 `Ok(())`，无返回载荷——回退后的
    /// 会话状态经既有读路径获取：`list_messages` 看剩余消息流，`get_session` 看重算后的
    /// session 行；目标用户消息的本体内容调用方本就持有（回退点由调用方选定）。
    ///
    /// # 运行态责任边界
    ///
    /// 本门面是纯存储操作，**不校验该 session 是否有活跃 turn**——若 turn 正在运行，
    /// 其后续落库会与回退结果竞争（回退被新写入部分抵消、重算计数漂移）。
    /// 需要安全回退的调用方应先经运行时门面 [`App::stop_session`](crate::App::stop_session)
    /// 屏障停 turn 再回退——「先停后滚」的顺序是回退安全性的承重前提，不可倒置。
    ///
    /// # 错误
    /// - [`fuyao_session::SessionError::NotFound`]：session_id 不存在，或 target_seq
    ///   在该 session 中无对应消息
    /// - [`fuyao_session::SessionError::InvalidCutTarget`]：目标非 user 且非
    ///   compaction（assistant / tool 中间态）
    pub async fn rollback_session(
        &self,
        session_id: &str,
        target_seq: i64,
    ) -> Result<(), fuyao_session::SessionError> {
        self.store.rollback_to(session_id, target_seq).await
    }

    // ── 会话派生（fork）──────────────────────────────────────────

    /// 把会话派生到目标消息之前（复制目标之前的全部消息到新独立会话，源会话不动）
    ///
    /// 复用存储层单事务原子执行体 [`SessionStore::fork_to`](fuyao_session::SessionStore::fork_to)
    /// （建新会话行 + 复制 `seq < target` 的消息 + 按复制结果聚合重算分支元数据），
    /// 返回新会话 id。分支的后续状态经既有读路径获取：`list_messages` 浏览分支消息，
    /// `get_session` 看分支元数据；继续对话经运行时门面
    /// [`App::resume_session`](crate::App::resume_session) 激活分支。
    ///
    /// # 切割语义
    ///
    /// 目标必须是 user 消息或 compaction 消息（assistant / tool 中间态拒绝）；
    /// 分支 = 目标消息之前的全部消息（含压缩前旧消息与更早的压缩边界）。
    /// 同一目标点上，回退删源会话的目标及其后消息，派生把目标之前的消息落成
    /// 新分支、源会话原样不动——分支消息集即回退后源会话的剩余消息集，
    /// 派生是回退的非破坏版本。
    ///
    /// # 运行态责任边界
    ///
    /// 派生无需先停 turn：消息 seq 单调递增且插入后不改，活跃 turn 的落库只发生
    /// 在目标之后，分支快照（`seq < target`）不受影响。
    ///
    /// # 错误
    /// - [`fuyao_session::SessionError::NotFound`]：session_id 不存在，或 target_seq
    ///   在该 session 中无对应消息
    /// - [`fuyao_session::SessionError::InvalidCutTarget`]：目标非 user 且非
    ///   compaction（assistant / tool 中间态）
    pub async fn fork_session(
        &self,
        session_id: &str,
        target_seq: i64,
    ) -> Result<String, fuyao_session::SessionError> {
        self.store.fork_to(session_id, target_seq).await
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
    /// 返回 [`MessagePage`](fuyao_api::MessagePage) 信封：`has_more` 判是否还有更早页，
    /// `next_cursor` 给翻页锚点（本页最旧消息的 seq，直接回传作下次 `before_seq`）。
    /// 游标分页不提供总数——消息是持续追加的流，total 会在新消息到达时过时、误导前端。
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
    ) -> Result<fuyao_api::MessagePage, fuyao_session::SessionError> {
        let limit = limit.unwrap_or(Self::DEFAULT_MESSAGE_PAGE_SIZE).max(1);
        let messages = self
            .store
            .list_messages_before(session_id, before_seq, limit)
            .await?;

        // has_more 由「本页条数 == limit」推导（满页才可能还有更多）。
        // next_cursor 取 messages（seq 倒序）末条——即本页最旧一条的 seq，作下次 before_seq。
        let has_more = messages.len() as i64 == limit;
        let next_cursor = if has_more {
            messages.last().map(|m| m.seq)
        } else {
            None
        };
        Ok(fuyao_api::MessagePage {
            items: messages,
            has_more,
            next_cursor,
        })
    }

    /// 历史消息投影成事件流（历史回放，seq 正序）
    ///
    /// 与 [`list_messages`](Self::list_messages) 同源取数（同游标、同分页），但把存储
    /// [`Message`] 投影成与实时流同构的 [`OutputEvent`]——前端历史回放与实时流共用一套
    /// 渲染逻辑，无需区分数据来源。转换由 [`fuyao_core::messages_to_events`] 承担
    /// （含 tool_calls 嵌套 → 扁平的逆向、seq 倒序翻正序），映射知识归 core 的
    /// history 模块——与「事件 → Message 落库」的正向映射同居一处，双向单点同步。
    ///
    /// 返回 [`EventPage`](fuyao_api::EventPage) 信封：`has_more` / `next_cursor` 直接复用
    /// [`list_messages`](Self::list_messages) 的推导（同源同游标），仅把 `items` 投影成
    /// `events`。游标分页不提供总数——消息是持续追加的流，total 会在新消息到达时过时。
    ///
    /// # 参数
    /// 同 [`list_messages`](Self::list_messages)。
    pub async fn list_events(
        &self,
        session_id: &str,
        before_seq: Option<i64>,
        limit: Option<i64>,
    ) -> Result<fuyao_api::EventPage, fuyao_session::SessionError> {
        // 复用 list_messages 的取数 + 游标推导，避免两处重复实现 limit/has_more/cursor 逻辑。
        let page = self.list_messages(session_id, before_seq, limit).await?;
        let events = fuyao_core::messages_to_events(page.items);
        Ok(fuyao_api::EventPage {
            events,
            has_more: page.has_more,
            next_cursor: page.next_cursor,
        })
    }
}
