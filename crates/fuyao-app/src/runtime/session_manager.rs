//! 会话管理门面：会话检索 / 浏览 / 元数据编辑的接口
//!
//! 与 [`crate::App`]（运行时交互门面）平级正交：
//! - [`crate::App`] 管对话的进行（create / send / recv）
//! - [`SessionManager`] 管会话的检索 / 浏览 / 手动编辑（列会话 / 查历史 / 改标题）
//!
//! 两者共享同一份 [`fuyao_session::SessionStore`]（由装配层 [`crate::start`] 注入
//! `Arc` 克隆），各取所需：Engine 写（LLM 流程落库、自动标题生成）、SessionManager
//! 读查询 + 补引擎不做的「用户 / 应用手动编辑」写（如手改标题、回退）。
//! 通讯方式为直接异步方法调用——纯存储操作、不涉及 LLM、不需要流式产出，
//! 与 `App::create_session` 返 `SessionId` 同属「管理型同步方法」，不走消息总线。
//!
//! # 写能力边界
//!
//! 本门面只暴露**适合外部编辑**的字段。引擎内核的自动写（create / 压缩后重建
//! system_prompt / 标题异步生成 / end_session）是其 ReAct 循环与 session task
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
    /// 本方法供应用 / 用户覆盖；两者最终都落同一个单字段 UPDATE，最后一个写生效，单字段原子。
    ///
    /// 透传 [`SessionStore::update_title`](fuyao_session::SessionStore::update_title)：
    /// EXISTS 校验后单字段 UPDATE，不动其他字段、不动 messages 表。
    ///
    /// # 返回
    /// - `Ok(())`：标题已更新
    /// - `Err(SessionError::NotFound)`：session_id 在数据库中不存在
    pub async fn update_title(
        &self,
        session_id: &str,
        new_title: &str,
    ) -> Result<(), fuyao_session::SessionError> {
        self.store.update_title(session_id, new_title).await
    }

    // ── 会话删除 ───────────────────────────────────────────────

    /// 删除会话（cascade 删消息 + 任务列表 + 会话行）
    ///
    /// 透传 [`SessionStore::delete`](fuyao_session::SessionStore::delete)：单事务内删
    /// todos + messages + sessions，三者要么全删要么全留。
    ///
    /// # 返回
    /// - `Ok(true)`：会话存在，删除成功
    /// - `Ok(false)`：会话不存在（无 session 行被删，但该 id 的残留 todos / messages 仍被清理）
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
    /// - [`fuyao_session::SessionError::InvalidRollbackTarget`]：目标非 user 且非
    ///   compaction（assistant / tool 中间态）
    pub async fn rollback_session(
        &self,
        session_id: &str,
        target_seq: i64,
    ) -> Result<(), fuyao_session::SessionError> {
        self.store.rollback_to(session_id, target_seq).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::{Message, Session};
    use fuyao_session::CompressionReason;

    /// 构造临时 SessionManager（隔离的临时目录 + 临时 store）
    async fn temp_manager() -> SessionManager {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let db_path = dir.path().join("test.db");
        // forget 让目录留到进程结束（SessionManager 跨 await 持有 store，dir 必须存活）
        std::mem::forget(dir);
        let store = Arc::new(SessionStore::new(db_path).await.expect("创建存储失败"));
        SessionManager::new(store)
    }

    /// 创建一个带 workspace 的 session 并落库
    async fn seed_session(manager: &SessionManager, workspace: Option<&str>) -> String {
        let mut session = Session::new(workspace.map(str::to_string), None, None);
        manager.store.create_with_retry(&mut session).await.unwrap();
        session.id
    }

    /// 创建一个挂在指定父会话下的子会话并落库
    ///
    /// `started_at` 可控（连同 `last_active_at` 一并设定），供按创建序排列的断言用。
    async fn seed_child_session(
        manager: &SessionManager,
        parent_id: &str,
        started_at: f64,
    ) -> String {
        let mut session = Session::new(None, None, None);
        session.parent_session_id = Some(parent_id.to_string());
        session.started_at = started_at;
        session.last_active_at = started_at;
        manager.store.create_with_retry(&mut session).await.unwrap();
        session.id
    }

    /// 给 session 追加一条 user 消息并落库
    async fn seed_user_message(manager: &SessionManager, session_id: &str, content: &str) {
        let mut msg = Message::user(content.to_string());
        manager
            .store
            .insert_message(session_id, &mut msg)
            .await
            .unwrap();
    }

    /// 给 session 追加一条 assistant 消息并落库
    async fn seed_assistant_message(manager: &SessionManager, session_id: &str, content: &str) {
        let mut msg = Message::assistant(Some(content.to_string()));
        manager
            .store
            .insert_message(session_id, &mut msg)
            .await
            .unwrap();
    }

    // ===== SessionPage：OFFSET 分页 + total =====

    #[tokio::test]
    async fn list_sessions_returns_page_with_total_and_echo_params() {
        let manager = temp_manager().await;
        for i in 0..3 {
            seed_session(&manager, Some("/proj-a")).await;
            let _ = i;
        }
        seed_session(&manager, Some("/proj-b")).await;

        // 第一页：limit=2, offset=0 → 2 条，total=4（全部）
        let page = manager.list_sessions(None, 2, 0).await.unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.total, 4, "total 应为过滤前的全部会话数");
        assert_eq!(page.limit, 2);
        assert_eq!(page.offset, 0);
    }

    #[tokio::test]
    async fn list_sessions_total_respects_workspace_filter() {
        let manager = temp_manager().await;
        for _ in 0..2 {
            seed_session(&manager, Some("/proj-a")).await;
        }
        seed_session(&manager, Some("/proj-b")).await;
        seed_session(&manager, None).await;

        // workspace 过滤：/proj-a 有 2 条，total=2
        let page = manager.list_sessions(Some("/proj-a"), 10, 0).await.unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.total, 2, "total 应与 workspace 过滤后的条数一致");
        // 无 workspace 的会话不在 /proj-a 过滤范围内
        assert!(
            page.items
                .iter()
                .all(|s| s.workspace.as_deref() == Some("/proj-a"))
        );
    }

    #[tokio::test]
    async fn list_sessions_offset_advances_pages() {
        let manager = temp_manager().await;
        for _ in 0..3 {
            seed_session(&manager, None).await;
        }

        let p1 = manager.list_sessions(None, 2, 0).await.unwrap();
        let p2 = manager.list_sessions(None, 2, 2).await.unwrap();
        assert_eq!(p1.items.len(), 2);
        assert_eq!(p2.items.len(), 1, "第三页只剩 1 条");
        // 两页 id 不重叠
        let p1_ids: Vec<&str> = p1.items.iter().map(|s| s.id.as_str()).collect();
        let p2_ids: Vec<&str> = p2.items.iter().map(|s| s.id.as_str()).collect();
        assert!(p1_ids.iter().all(|id| !p2_ids.contains(id)));
    }

    // ===== 主列表排除子会话 + 按父列举 + child_count =====

    #[tokio::test]
    async fn list_sessions_excludes_child_sessions() {
        // 主列表只含顶层会话：子会话不占列表行，也不占分页 total
        let manager = temp_manager().await;
        let main_a = seed_session(&manager, None).await;
        let main_b = seed_session(&manager, None).await;
        seed_child_session(&manager, &main_a, 100.0).await;
        seed_child_session(&manager, &main_a, 200.0).await;
        seed_child_session(&manager, &main_b, 300.0).await;

        let page = manager.list_sessions(None, 100, 0).await.unwrap();
        assert_eq!(page.items.len(), 2, "三个子会话不应出现在主列表");
        assert!(
            page.items.iter().all(|s| s.parent_session_id.is_none()),
            "主列表行的 parent_session_id 应全为空"
        );
        let ids: Vec<&str> = page.items.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&main_a.as_str()));
        assert!(ids.contains(&main_b.as_str()));
        assert_eq!(page.total, 2, "total 只计主会话");
    }

    #[tokio::test]
    async fn list_sessions_child_count_matches_actual_children() {
        // 主列表行携带的 child_count 与该主会话实际子会话数一致，无子为 0
        let manager = temp_manager().await;
        let with_children = seed_session(&manager, None).await;
        let childless = seed_session(&manager, None).await;
        seed_child_session(&manager, &with_children, 100.0).await;
        seed_child_session(&manager, &with_children, 200.0).await;

        let page = manager.list_sessions(None, 100, 0).await.unwrap();
        let count_of = |id: &str| {
            page.items
                .iter()
                .find(|s| s.id == id)
                .unwrap_or_else(|| panic!("主列表应含 {id}"))
                .child_count
        };
        assert_eq!(count_of(&with_children), 2, "两个子会话应计为 2");
        assert_eq!(count_of(&childless), 0, "无子会话应计为 0");
    }

    #[tokio::test]
    async fn list_child_sessions_returns_all_in_creation_order() {
        // 按父列举：全量返回该父的子会话，按创建序（started_at 升序）排列
        let manager = temp_manager().await;
        let parent = seed_session(&manager, None).await;
        // 派生顺序故意打乱（先 300 再 100 后 200），断言输出按创建时间升序
        let third = seed_child_session(&manager, &parent, 300.0).await;
        let first = seed_child_session(&manager, &parent, 100.0).await;
        let second = seed_child_session(&manager, &parent, 200.0).await;

        let children = manager.list_child_sessions(&parent).await.unwrap();
        assert_eq!(children.len(), 3, "该父的全部子会话全量返回，无分页信封");
        let ids: Vec<&str> = children.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![first.as_str(), second.as_str(), third.as_str()],
            "按创建序（started_at 升序）排列"
        );
        assert!(
            children
                .iter()
                .all(|s| s.parent_session_id.as_deref() == Some(parent.as_str()))
        );
    }

    #[tokio::test]
    async fn list_child_sessions_scoped_to_parent_and_empty_cases() {
        // 只列目标父的子会话（他父的子不混入）；无子父返回空列表
        let manager = temp_manager().await;
        let parent_a = seed_session(&manager, None).await;
        let parent_b = seed_session(&manager, None).await;
        seed_child_session(&manager, &parent_a, 100.0).await;
        seed_child_session(&manager, &parent_b, 200.0).await;

        let children_a = manager.list_child_sessions(&parent_a).await.unwrap();
        assert_eq!(children_a.len(), 1, "只含 parent_a 的子会话");
        assert_eq!(
            children_a[0].parent_session_id.as_deref(),
            Some(parent_a.as_str())
        );

        // 无子的父 → 空列表
        let parent_c = seed_session(&manager, None).await;
        assert!(
            manager
                .list_child_sessions(&parent_c)
                .await
                .unwrap()
                .is_empty()
        );
    }

    // ===== get_session：单会话主键直读 =====

    #[tokio::test]
    async fn get_session_returns_session_by_id() {
        let manager = temp_manager().await;
        let sid = seed_session(&manager, Some("/proj-a")).await;

        let loaded = manager.get_session(&sid).await.unwrap().expect("应命中");
        assert_eq!(loaded.id, sid);
        assert_eq!(loaded.workspace.as_deref(), Some("/proj-a"));
    }

    #[tokio::test]
    async fn get_session_unknown_id_returns_none() {
        let manager = temp_manager().await;
        assert!(manager.get_session("no-such-id").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_session_reflects_db_derived_counters() {
        // 落一条消息后 message_count 应为 1——证明返回的是 DB 最新读数（计数由
        // insert_message 事务内累加），而非创建时的快照
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;
        seed_user_message(&manager, &sid, "你好").await;

        let loaded = manager.get_session(&sid).await.unwrap().expect("应命中");
        assert_eq!(loaded.message_count, 1);
    }

    // ===== MessagePage：游标分页 + has_more / next_cursor =====

    #[tokio::test]
    async fn list_messages_first_page_has_more_and_cursor() {
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;
        // 插 3 条消息，limit=2 → 首页 2 条，has_more=true
        for i in 1..=3 {
            seed_user_message(&manager, &sid, &format!("msg{i}")).await;
        }

        let page = manager.list_messages(&sid, None, Some(2)).await.unwrap();
        assert_eq!(page.items.len(), 2, "首页应满 2 条");
        assert!(page.has_more, "还有更早消息，has_more 应为 true");
        // items 按 seq 倒序（新→旧）：取最新 2 条 = seq3、seq2
        assert_eq!(page.items[0].seq, 3);
        assert_eq!(page.items[1].seq, 2);
        // next_cursor = 本页最旧条 seq（末条）= 2，作下次 before_seq
        assert_eq!(page.next_cursor, Some(2));
    }

    #[tokio::test]
    async fn list_messages_next_cursor_drives_next_page() {
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;
        for i in 1..=3 {
            seed_user_message(&manager, &sid, &format!("msg{i}")).await;
        }

        // 第一页取最新 2 条（seq3, seq2），next_cursor=2
        let p1 = manager.list_messages(&sid, None, Some(2)).await.unwrap();
        assert_eq!(p1.next_cursor, Some(2));

        // 用 next_cursor 翻第二页（seq<2 → seq1）
        let p2 = manager
            .list_messages(&sid, p1.next_cursor, Some(2))
            .await
            .unwrap();
        assert_eq!(p2.items.len(), 1, "第二页只剩 seq1 一条");
        assert_eq!(p2.items[0].seq, 1);
        assert!(!p2.has_more, "到底了，has_more 应为 false");
        assert_eq!(p2.next_cursor, None);
    }

    #[tokio::test]
    async fn list_messages_last_page_has_no_more() {
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;
        for i in 1..=3 {
            seed_user_message(&manager, &sid, &format!("msg{i}")).await;
        }

        // 第一页满 2 条（seq3、seq2），has_more=true，cursor=2
        let p1 = manager.list_messages(&sid, None, Some(2)).await.unwrap();
        assert_eq!(p1.items.len(), 2);
        assert!(p1.has_more);
        assert_eq!(p1.next_cursor, Some(2));

        // 翻第二页（seq<2 → seq1，仅 1 条 < limit）→ 真正到底，has_more=false
        let p2 = manager
            .list_messages(&sid, p1.next_cursor, Some(2))
            .await
            .unwrap();
        assert_eq!(p2.items.len(), 1, "第二页只剩 seq1");
        assert!(!p2.has_more, "不足一页，确无更多");
        assert_eq!(p2.next_cursor, None);
    }

    // ===== EventPage：游标分页 + has_more / next_cursor =====

    #[tokio::test]
    async fn list_events_first_page_has_more_and_cursor() {
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;
        // 插 3 条消息，limit=2 → 首页 2 条，has_more=true
        for i in 1..=3 {
            seed_user_message(&manager, &sid, &format!("msg{i}")).await;
        }

        let page = manager.list_events(&sid, None, Some(2)).await.unwrap();
        assert_eq!(page.events.len(), 2, "首页应满 2 条");
        assert!(page.has_more, "还有更早消息，has_more 应为 true");
        // next_cursor = 本页最旧消息的 seq（首页取 seq3、seq2，最旧为 seq2）
        assert_eq!(page.next_cursor, Some(2));
    }

    #[tokio::test]
    async fn list_events_next_cursor_drives_next_page() {
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;
        for i in 1..=3 {
            seed_user_message(&manager, &sid, &format!("msg{i}")).await;
        }

        // 第一页取最新 2 条（seq3、seq2），next_cursor=2
        let p1 = manager.list_events(&sid, None, Some(2)).await.unwrap();
        assert_eq!(p1.next_cursor, Some(2));

        // 用 next_cursor 翻第二页（seq<2 → seq1）
        let p2 = manager
            .list_events(&sid, p1.next_cursor, Some(2))
            .await
            .unwrap();
        assert_eq!(p2.events.len(), 1, "第二页只剩 seq1 一条");
        assert!(!p2.has_more, "到底了，has_more 应为 false");
        assert_eq!(p2.next_cursor, None);
    }

    #[tokio::test]
    async fn list_events_last_page_has_no_more() {
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;
        for i in 1..=3 {
            seed_user_message(&manager, &sid, &format!("msg{i}")).await;
        }

        // 第一页满 2 条（seq3、seq2），has_more=true，cursor=2
        let p1 = manager.list_events(&sid, None, Some(2)).await.unwrap();
        assert_eq!(p1.events.len(), 2);
        assert!(p1.has_more);
        assert_eq!(p1.next_cursor, Some(2));

        // 翻第二页（seq<2 → seq1，仅 1 条 < limit）→ 真正到底，has_more=false
        let p2 = manager
            .list_events(&sid, p1.next_cursor, Some(2))
            .await
            .unwrap();
        assert_eq!(p2.events.len(), 1, "第二页只剩 seq1");
        assert!(!p2.has_more, "不足一页，确无更多");
        assert_eq!(p2.next_cursor, None);
    }

    #[tokio::test]
    async fn list_events_empty_session_returns_empty_page() {
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;

        let page = manager.list_events(&sid, None, Some(10)).await.unwrap();
        assert!(page.events.is_empty());
        assert!(!page.has_more);
        assert_eq!(page.next_cursor, None);
    }

    // ===== update_title：透传 store.update_title（单字段 UPDATE，与引擎自动生成正交）=====

    #[tokio::test]
    async fn update_title_succeeds_and_persists() {
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;

        manager.update_title(&sid, "Rust 异步讨论").await.unwrap();

        let loaded = manager.store.get(&sid).await.unwrap().unwrap();
        assert_eq!(loaded.title.as_deref(), Some("Rust 异步讨论"));
        // 单字段 UPDATE 不触碰计数字段
        assert_eq!(loaded.compression_count, 0);
        assert!(loaded.last_compacted_seq.is_none());
    }

    #[tokio::test]
    async fn update_title_errors_on_missing_session() {
        let manager = temp_manager().await;

        let result = manager.update_title("nonexistent", "标题").await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            fuyao_session::SessionError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn update_title_overrides_engine_generated_default() {
        // seed_session 产出默认标题 "新会话"（Session::new 的兜底值），手改后验证覆盖
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;

        let before = manager.store.get(&sid).await.unwrap().unwrap();
        assert_eq!(before.title.as_deref(), Some("新会话"));

        manager.update_title(&sid, "用户自定义标题").await.unwrap();

        let after = manager.store.get(&sid).await.unwrap().unwrap();
        assert_eq!(after.title.as_deref(), Some("用户自定义标题"));
    }

    // ===== delete_session：透传 store.delete（cascade 删 todos + messages + sessions）=====

    #[tokio::test]
    async fn delete_session_returns_true_and_removes_session() {
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;
        seed_user_message(&manager, &sid, "对话").await;

        // 删除前会话存在且有消息
        let before = manager.list_messages(&sid, None, Some(10)).await.unwrap();
        assert_eq!(before.items.len(), 1);

        let deleted = manager.delete_session(&sid).await.unwrap();
        assert!(deleted, "存在的会话应返回 true");

        // 删除后会话在列表中消失
        let after = manager.list_sessions(None, 100, 0).await.unwrap();
        assert!(
            after.items.iter().all(|s| s.id != sid),
            "删除后列表不应再包含该会话"
        );
    }

    #[tokio::test]
    async fn delete_session_returns_false_for_nonexistent() {
        let manager = temp_manager().await;

        // 不存在的会话 id：返回 false（不报错）
        let deleted = manager.delete_session("nonexistent").await.unwrap();
        assert!(!deleted, "不存在的会话应返回 false");
    }

    #[tokio::test]
    async fn delete_session_cascades_todos_no_orphans() {
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;

        // 写一条任务（经 store 的 todo 能力，验证删除级联无残留）
        manager
            .store
            .write_todos(
                &sid,
                vec![fuyao_api::TodoItem {
                    id: "1".to_string(),
                    content: "任务".to_string(),
                    status: "pending".to_string(),
                }],
            )
            .await
            .unwrap();
        assert_eq!(
            manager.store.read_todos(&sid).await.unwrap().len(),
            1,
            "删除前应有 1 条任务"
        );

        // 删会话 → 任务列表随之清空，无孤儿
        assert!(manager.delete_session(&sid).await.unwrap());
        assert!(
            manager.store.read_todos(&sid).await.unwrap().is_empty(),
            "删会话后任务列表应清空"
        );
    }

    // ===== rollback_session：直调存储层回退执行体 =====

    #[tokio::test]
    async fn rollback_session_deletes_target_and_after_recounts() {
        // 场景：u1, a1, u2, a2 → 回退到 u2 → 删 u2, a2；状态经读路径对齐
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;
        seed_user_message(&manager, &sid, "u1").await;
        seed_assistant_message(&manager, &sid, "a1").await;
        seed_user_message(&manager, &sid, "u2").await;
        seed_assistant_message(&manager, &sid, "a2").await;

        // 回退目标取 u2 的 seq（插入顺序 1/2/3/4，u2 = seq3）
        manager.rollback_session(&sid, 3).await.unwrap();

        // DB：目标 u2 与其后的 a2 一并删除，只剩 u1, a1（seq 倒序，最新在前）
        let msgs = manager
            .list_messages(&sid, None, Some(10))
            .await
            .unwrap()
            .items;
        assert_eq!(msgs.len(), 2, "目标及其后的消息一并删除");
        assert_eq!(msgs[0].seq, 2, "最新一条是 a1");

        // session 行 count 已被事务重算
        let loaded = manager.store.get(&sid).await.unwrap().unwrap();
        assert_eq!(loaded.message_count, 2);
    }

    #[tokio::test]
    async fn rollback_session_compaction_target_discards_the_compaction() {
        // compaction 目标：压缩消息本体及之后一并删除，元数据随之清空
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;
        seed_user_message(&manager, &sid, "u1").await;
        let comp_seq = manager
            .store
            .mark_compaction(&sid, "摘要".to_string(), CompressionReason::Auto)
            .await
            .unwrap();
        seed_user_message(&manager, &sid, "u2").await;

        manager.rollback_session(&sid, comp_seq).await.unwrap();

        // 压缩消息与 u2 都被删，只剩 u1
        let msgs = manager
            .list_messages(&sid, None, Some(10))
            .await
            .unwrap()
            .items;
        assert_eq!(msgs.len(), 1, "压缩消息本体与其后的 u2 一并删除");
        assert_eq!(msgs[0].seq, 1, "只剩最早的 u1");

        // 压缩元数据已清空
        let loaded = manager.store.get(&sid).await.unwrap().unwrap();
        assert_eq!(loaded.last_compacted_seq, None);
        assert_eq!(loaded.compression_count, 0);
    }

    #[tokio::test]
    async fn rollback_session_rejects_assistant_target_and_keeps_db() {
        // 非法目标（assistant 中间态）→ Err(InvalidRollbackTarget)，DB 不变
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;
        seed_user_message(&manager, &sid, "u1").await;
        seed_assistant_message(&manager, &sid, "a1").await;
        let before = manager
            .list_messages(&sid, None, Some(10))
            .await
            .unwrap()
            .items
            .len();

        // assistant 消息的 seq = 2
        let result = manager.rollback_session(&sid, 2).await;
        assert!(matches!(
            result,
            Err(fuyao_session::SessionError::InvalidRollbackTarget(_))
        ));

        let after = manager
            .list_messages(&sid, None, Some(10))
            .await
            .unwrap()
            .items
            .len();
        assert_eq!(before, after, "非法回退不应改动 DB");
    }

    #[tokio::test]
    async fn rollback_session_missing_session_errors() {
        let manager = temp_manager().await;
        let result = manager.rollback_session("nonexistent", 1).await;
        assert!(matches!(
            result,
            Err(fuyao_session::SessionError::NotFound(_))
        ));
    }
}
